// Isolated real-process acceptance test. Node is a development client, never a server dependency.
const fs=require('node:fs'),path=require('node:path'),net=require('node:net'),assert=require('node:assert/strict');
const {spawn,execFileSync}=require('node:child_process');const {randomUUID}=require('node:crypto');
const [binary,root,reportFile]=process.argv.slice(2);assert(binary&&root&&reportFile);assert(!fs.existsSync(root),'Use a new, isolated directory');
fs.mkdirSync(root,{recursive:true});const workspace=path.join(root,'workspace'),privateDir=path.join(root,'private');fs.mkdirSync(workspace);
const sleep=ms=>new Promise(r=>setTimeout(r,ms));
async function until(check,label,seconds=12){const end=Date.now()+seconds*1000;while(Date.now()<end){const value=await check();if(value)return value;await sleep(30);}throw Error('Timed out: '+label);}
async function unusedPort(){const s=net.createServer();await new Promise(r=>s.listen(0,'127.0.0.1',r));const port=s.address().port;await new Promise(r=>s.close(r));return port;}
function cli(args){return execFileSync(binary,args,{windowsHide:true,encoding:'utf8',stdio:['ignore','pipe','pipe']});}
let server,base,token,config,id=0;
function launch(){server=spawn(binary,['--config',config,'serve'],{windowsHide:true,stdio:['ignore','ignore','pipe']});server.stderr.on('data',()=>{});return server;}
async function terminate(){if(!server||server.exitCode!==null)return;const current=server;const exited=new Promise((resolve,reject)=>{current.once('exit',resolve);current.once('error',reject);});current.kill('SIGKILL');await exited;}
async function rpc(method,params){const r=await fetch(base+'/mcp',{method:'POST',headers:{Authorization:'Bearer '+token,'Content-Type':'application/json',Accept:'application/json, text/event-stream','MCP-Protocol-Version':'2025-11-25'},body:JSON.stringify({jsonrpc:'2.0',id:++id,method,params}),signal:AbortSignal.timeout(15000)});assert.equal(r.status,200);const body=await r.json();assert(!body.error,JSON.stringify(body));return body.result;}
async function call(tool,args,allowError=false){const r=await rpc('tools/call',{name:tool,arguments:args});if(!allowError)assert(!r.isError,JSON.stringify(r));return r;}
async function run(tool,args,allowError=false){return(await call(tool,args,allowError)).structuredContent;}
async function ready(){await until(async()=>{try{return(await fetch(base+'/health',{headers:{Authorization:'Bearer '+token},signal:AbortSignal.timeout(500)})).ok;}catch{return false;}},'Reach listener');}
function alive(pid){try{process.kill(pid,0);return true;}catch{return false;}}
async function screen(terminal_id,needle){return until(async()=>{const r=await run('TerminalRead',{terminal_id});return r.screen?.lines.join('\n').includes(needle)&&r;},'terminal '+needle);}
async function main(){
 const port=await unusedPort();base='http://127.0.0.1:'+port;
 cli(['init','--directory',privateDir,'--cwd',workspace,'--listen','127.0.0.1:'+port,'--no-admin']);config=path.join(privateDir,'config.json');
 const tokenFile=path.join(privateDir,'test.token');cli(['--config',config,'token','issue','--principal','recovery-test','--profile','native','--ttl-hours','1','--token-file',tokenFile]);token=fs.readFileSync(tokenFile,'utf8').trim();
 launch();await ready();const info=await run('Info',{});assert.equal(info.terminal_backend.implementation,'microsoft_redistributable_conpty');assert(info.terminal_backend.alternate_screen);
 const terminalRequest=randomUUID();const terminalArgs={operation_id:terminalRequest,env:{REACH_TEST_ID:'persistent'}};
 const terminal=await run('TerminalOpen',terminalArgs);assert.equal(terminal.state,'running');
 const descendantFile=path.join(workspace,'descendant.pid');
 const text=`$global:reachState='KEPT'; $c=Start-Process -FilePath powershell.exe -ArgumentList '-NoProfile','-NonInteractive','-Command','Start-Sleep -Seconds 120' -WindowStyle Hidden -PassThru; Set-Content -LiteralPath '${descendantFile.replaceAll("'","''")}' -Value $c.Id; Write-Output ('CHILD_'+'ALIVE')\r`;
 await run('TerminalInput',{terminal_id:terminal.terminal_id,input_id:randomUUID(),input:{kind:'text',text:text}});await screen(terminal.terminal_id,'CHILD_ALIVE');
 const descendant=Number(fs.readFileSync(descendantFile,'utf8').trim());assert(alive(descendant));
 // Each fetch is an independent HTTP request. Screen and shell state survive connection changes.
 await run('TerminalInput',{terminal_id:terminal.terminal_id,input:{kind:'text',text:"Write-Output ($global:reachState+'_AFTER_RECONNECT')\r"}});await screen(terminal.terminal_id,'KEPT_AFTER_RECONNECT');
 const commandId=randomUUID();const count=path.join(workspace,'command-count.txt');
 const command=`Add-Content -LiteralPath '${count.replaceAll("'","''")}' -Value 'executed'; [Console]::WriteLine('COMMAND_READY'); Start-Sleep -Seconds 120`;
 const commandArgs={operation_id:commandId,command,wait_seconds:0};const commandTask=await run('Shell',commandArgs);assert.equal(commandTask.task_id,commandId);
 await until(()=>fs.existsSync(count),'one-shot command startup');assert.equal(fs.readFileSync(count,'utf8').trim(),'executed');
 const create=await run('Write',{path:'recover.txt',update:{mode:'create',content:'before\r\n'}});const read=await run('Read',{paths:['recover.txt']});
 const replace=await run('Write',{path:'recover.txt',update:{mode:'replace',content:'after\r\n',expected_sha256:read.files[0].sha256}});assert.equal(replace.status,'completed');
 await terminate();await until(()=>!alive(descendant),'PTY descendant cleanup after executor death');
 assert(!alive(terminal.pid),'PTY shell survived executor death');
 launch();await ready();
 const recoveredTerminal=await run('TerminalOpen',terminalArgs,true);assert.equal(recoveredTerminal.terminal_id,terminal.terminal_id);assert.equal(recoveredTerminal.state,'interrupted');assert(recoveredTerminal.output_expired);
 const expired=await run('TerminalRead',{terminal_id:terminal.terminal_id},true);assert.equal(expired.error.code,'output_expired');
 const recoveredCommand=await run('Shell',commandArgs,true);assert.equal(recoveredCommand.task_id,commandTask.task_id);assert.equal(recoveredCommand.status,'interrupted');assert.equal(fs.readFileSync(count,'utf8').trim(),'executed');
 const inspected=await run('Recover',{source_task_id:replace.task_id});assert(inspected.files[0].matches_proposed);assert(inspected.recorded_receipt.success);
 const versions=Object.fromEntries(inspected.files.map(f=>[f.path,f.current]));
 const restored=await run('Recover',{source_task_id:replace.task_id,recovery:{action:'restore',versions}});assert.equal(restored.status,'completed');assert.equal(fs.readFileSync(path.join(workspace,'recover.txt'),'utf8'),'before\r\n');
 assert.equal((await run('Tasks',{task_id:create.task_id})).status,'completed');
 const report={ok:true,version:info.version,real_http_reconnect:true,executor_force_killed:true,terminal_tree_terminated:true,terminal_restart_interrupted:true,command_not_replayed:true,durable_file_recovery_after_restart:true,backend:info.terminal_backend.implementation,production_untouched:true};
 fs.writeFileSync(reportFile,JSON.stringify(report,null,2));console.log(JSON.stringify(report));
}
main().catch(e=>{console.error(e.message);process.exitCode=1;}).finally(terminate);
