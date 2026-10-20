// DOM behavior test, not a browser/rendering test. jsdom is a development-only
// dependency supplied by the test environment; Reach has no JS runtime dependency.
const fs=require('fs'),path=require('path'),assert=require('node:assert/strict'),cp=require('child_process');
const {JSDOM}=require('jsdom');
const [binary,configFile,base,reportPath]=process.argv.slice(2);
assert(binary&&configFile&&base&&reportPath,'Usage: node test-ui.cjs <reach.exe> <config.json> <admin-url> <report.json>');
const conf=JSON.parse(fs.readFileSync(configFile));
const url=cp.execFileSync(binary,['--config',configFile,'ui','--no-open'],{encoding:'utf8',windowsHide:true}).trim();
const source=path.resolve(__dirname,'..');
const dom=new JSDOM(fs.readFileSync(path.join(source,'web/index.html'),'utf8'),{url,runScripts:'outside-only',pretendToBeVisual:true});
const w=dom.window,d=w.document;let cookie='',copied='';const checks=[];
const requests=[];
w.fetch=async(route,options={})=>{
 const headers={...options.headers};if(cookie)headers.Cookie=cookie;if(options.method==='POST')headers.Origin=base;
 const response=await fetch(new URL(route,base),{...options,headers});
 const set=response.headers.get('set-cookie');if(set)cookie=set.split(';')[0];requests.push({route,status:response.status});return response;
};
Object.defineProperty(w.navigator,'clipboard',{value:{writeText:async text=>{copied=text;}}});
w.HTMLDialogElement.prototype.showModal=function(){this.open=true;};
w.HTMLDialogElement.prototype.close=function(){this.open=false;this.dispatchEvent(new w.Event('close'));};
const pause=ms=>new Promise(r=>setTimeout(r,ms));
async function until(check,message){const start=Date.now();while(!check()){if(Date.now()-start>5000)throw Error(message);await pause(20);}}
async function click(element){assert(element,'Missing UI control');element.click();await pause(30);}
(async()=>{
 try{
  w.eval(fs.readFileSync(path.join(source,'web/app.js'),'utf8'));
  await until(()=>!d.getElementById('workspace').hidden,'Login did not reveal workspace');
  assert.equal(w.location.hash,'');assert(d.getElementById('runtime').textContent.includes(conf.default_cwd));checks.push('One-time login removes URL fragment and reveals runtime');
  await click(d.getElementById('new-client'));assert(d.getElementById('create-dialog').open);
  const form=d.getElementById('client-form');form.elements.label.value='DOM test <img src=x onerror=alert(1)>';form.elements.access.value='read';form.elements.profile.value='compact';form.elements.roots.value=conf.default_cwd;
  form.dispatchEvent(new w.Event('submit',{bubbles:true,cancelable:true}));
  await until(()=>d.getElementById('secret-dialog').open,'Issuing did not show one-time configuration');
  const config=JSON.parse(d.getElementById('connection-config').value);assert(config.mcpServers.reach.headers.Authorization.startsWith('Bearer reach_'));assert.equal(config.mcpServers.reach.url,`http://${conf.listen}/mcp`);
  await click(d.getElementById('copy-config'));assert.equal(JSON.parse(copied).mcpServers.reach.url,config.mcpServers.reach.url);checks.push('Client form creates scoped text credential and copyable MCP config');
  await click(d.querySelector('[data-close="secret-dialog"]'));assert.equal(d.getElementById('connection-config').value,'');
  await until(()=>[...d.querySelectorAll('.client-title')].some(n=>n.textContent.includes('DOM test')),'New client did not appear');
  let row=[...d.querySelectorAll('.client')].find(n=>n.textContent.includes('DOM test'));assert.equal(row.querySelectorAll('img').length,0);assert(row.textContent.includes('项目只读'));checks.push('Untrusted client label renders as text; secret is cleared on close');
  await click(row.querySelector('[data-action="rotate"]'));assert(d.getElementById('confirm-dialog').open);await click(d.getElementById('confirm-accept'));
  await until(()=>d.getElementById('secret-dialog').open,'Rotation did not return a new config');assert.notEqual(JSON.parse(d.getElementById('connection-config').value).mcpServers.reach.headers.Authorization,config.mcpServers.reach.headers.Authorization);
  await click(d.querySelector('[data-close="secret-dialog"]'));checks.push('Rotation requires explicit confirmation and returns a new one-time config');
  row=[...d.querySelectorAll('.client')].find(n=>n.textContent.includes('DOM test')&&!n.textContent.includes('已撤销'));assert(row);
  await click(row.querySelector('[data-action="revoke"]'));await click(d.getElementById('confirm-accept'));
  await until(()=>![...d.querySelectorAll('.client')].some(n=>n.textContent.includes('DOM test')&&!n.textContent.includes('已撤销')),'Revoke did not update status');checks.push('Revocation confirmation updates client state');
  await click(d.getElementById('logout'));await until(()=>d.getElementById('workspace').hidden,'Logout did not finish');checks.push('Logout returns to locked state');
  assert(!requests.some(x=>x.status>=400));
  fs.writeFileSync(reportPath,JSON.stringify({ok:true,checks,requests:requests.length,renderingVerified:false,reason:'Browser interaction disabled by Work settings; DOM behavior and real local API verified'},null,2));
  console.log(JSON.stringify({ok:true,checks:checks.length,renderingVerified:false}));
 }finally{w.close();}
})().catch(e=>{console.error(e.message);process.exitCode=1;});
