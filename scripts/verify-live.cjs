// Verification client only; not required by Reach or any MCP host.
const fs=require('fs'),assert=require('node:assert/strict');
const [base,tokenFile,reportFile]=process.argv.slice(2);assert(base&&tokenFile&&reportFile);
const token=fs.readFileSync(tokenFile,'utf8').trim();let id=0;
async function rpc(method,params){const r=await fetch(base+'/mcp',{method:'POST',headers:{Authorization:'Bearer '+token,'Content-Type':'application/json',Accept:'application/json, text/event-stream','MCP-Protocol-Version':'2025-11-25'},body:JSON.stringify({jsonrpc:'2.0',id:++id,method,params})});assert.equal(r.status,200);const body=await r.json();assert(!body.error);return{body:body.result,headers:r.headers};}
async function run(tool,args){const r=(await rpc('tools/call',{name:'Run',arguments:{tool,arguments:args}})).body;assert(!r.isError,JSON.stringify(r));return r.structuredContent;}
(async()=>{
 const response=await fetch(base+'/health');assert.equal(response.status,401);
 const init=await rpc('initialize',{protocolVersion:'2025-11-25',capabilities:{},clientInfo:{name:'reach-release-verifier',version:'1'}});assert.equal(init.body.serverInfo.name,'reach');assert(!init.headers.get('Mcp-Session-Id'));assert(init.body.instructions.includes('ApplyPatch'));
 const compact=(await rpc('tools/list',{})).body.tools;assert.deepEqual(compact.map(t=>t.name),['Run','Discover']);const info=await run('Info',{});assert.equal(info.principal,'tokyo-client');
 const read=await run('Read',{paths:['reach-mcp/Cargo.toml']});assert(read.files[0].text.includes('reach-mcp'));
 const shell=await run('Shell',{command:'Write-Output ((Get-Location).Path); Write-Output "Reach live verification"'});assert.equal(shell.status,'completed');assert(shell.result.stdout.includes('Reach live verification'));
 const guideBytes=Buffer.byteLength(init.body.instructions),schemaBytes=Buffer.byteLength(JSON.stringify(compact));
 const report={ok:true,server:info.server,version:info.version,compact_tools:compact.map(t=>t.name),compact_schema_bytes:schemaBytes,initial_guide_bytes:guideBytes,default_cwd:info.default_cwd,authenticated:true,stateless:true,read:true,shell:true,base,modelHostPromptInjectionVerified:false};fs.writeFileSync(reportFile,JSON.stringify(report,null,2));console.log(JSON.stringify(report));
})().catch(e=>{console.error(e.message);process.exitCode=1;});
