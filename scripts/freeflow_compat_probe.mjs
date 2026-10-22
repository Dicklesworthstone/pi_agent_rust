/**
 * Supplementary regression probe for pi-freeflow 1.39.0 / issue #270.
 *
 * Prerequisites: Node.js >= 24 with --experimental-vm-modules, and Git.
 * Fetch the fixture into a NEW directory outside this repository:
 *
 *   git -c http.userAgent="pi_agent_rust-agent (+https://github.com/Dicklesworthstone/pi_agent_rust)" clone --no-checkout https://github.com/trefeon/pi-freeflow /path/to/pi-freeflow-fixture
 *   git -C /path/to/pi-freeflow-fixture checkout --detach c4370df7fd4d186d00a33e4fc177ea340abf08d3
 *
 * Run from the pi_agent_rust checkout:
 *
 *   node --experimental-vm-modules scripts/freeflow_compat_probe.mjs --upstream /path/to/pi-freeflow-fixture
 *   node --experimental-vm-modules scripts/freeflow_compat_probe.mjs --upstream /path/to/pi-freeflow-fixture --windows
 *
 * This reads the current Rust-embedded JavaScript verbatim, then evaluates the
 * pinned extension against controlled native bindings. HTTP responses are
 * explicit fixtures with pull credits; no host networking API or OS listener
 * is supplied. Extension filesystem reads stay within the upstream fixture,
 * while writes go to PiJS's in-memory filesystem. Process execution is denied.
 *
 * The Windows option projects Windows paths, platform and environment values;
 * it does not execute Windows or the Rust/QuickJS runtime. V8 vm contexts are
 * diagnostics, not a security boundary for arbitrary untrusted source. This
 * probe therefore requires the pinned, unmodified fixture and does not replace
 * the authoritative DSR gate: dsr quality --tool pi_agent_rust.
 */

import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import crypto from 'node:crypto';
import assert from 'node:assert/strict';
import {stripTypeScriptTypes} from 'node:module';
import {execFileSync} from 'node:child_process';
import {fileURLToPath} from 'node:url';
import {parseArgs} from 'node:util';

const {values: options}=parseArgs({
 options:{upstream:{type:'string'},repo:{type:'string'},windows:{type:'boolean'},help:{type:'boolean'}},
});
if(options.help){
 console.log('Usage: node --experimental-vm-modules scripts/freeflow_compat_probe.mjs --upstream /path/to/pinned/pi-freeflow [--windows] [--repo /path/to/pi_agent_rust]');
 process.exit(0);
}
if(!options.upstream)throw Error('--upstream is required; see the fixture checkout instructions at the top of this script.');
if(typeof vm.SourceTextModule!=='function')throw Error('Run Node.js with --experimental-vm-modules.');
const PI=fs.realpathSync(options.repo??fileURLToPath(new URL('..',import.meta.url)));
const UPSTREAM=fs.realpathSync(options.upstream);
const PIN='c4370df7fd4d186d00a33e4fc177ea340abf08d3';
const fixtureGit=(...args)=>execFileSync('git',['--no-optional-locks','-C',UPSTREAM,...args],{encoding:'utf8',stdio:['ignore','pipe','pipe']}).trim();
assert.equal(fixtureGit('rev-parse','HEAD'),PIN,'The upstream fixture must be pinned to '+PIN);
assert.equal(fixtureGit('diff','--no-ext-diff','--no-textconv','--name-only','HEAD','--'),'', 'The upstream fixture must have no tracked modifications.');
const EXTENSION='pi-freeflow-probe', SECRET='host-only-probe-secret';
const WINDOWS=options.windows??false;
const PKG=WINDOWS?'C:/Users/reporter/AppData/Roaming/npm/node_modules/pi-freeflow':'/fixture/node_modules/pi-freeflow';
const CWD=WINDOWS?'C:/Users/reporter/workspace':'/fixture/workspace';
const source=fs.readFileSync(PI+'/src/extensions_js.rs','utf8');
function rawLiteral(text,from=0){
 const pattern=/r(#{0,})"/g;pattern.lastIndex=from;const start=pattern.exec(text);
 if(!start)throw Error('Missing raw string after '+from);
 const body=start.index+start[0].length,end=text.indexOf('"'+start[1],body);
 if(end<0)throw Error('Unterminated raw string');
 return{value:text.slice(body,end),end:end+1+start[1].length};
}
function embeddedSources(){
 const start=source.indexOf('fn pi_bridge_js()'),end=source.indexOf('\n#[cfg(test)]',start),bridgeText=source.slice(start,end),chunks=[];
 let cursor=0;
 while((cursor=bridgeText.indexOf('compressed_js_literal!(',cursor))>=0){const raw=rawLiteral(bridgeText,cursor);chunks.push(raw.value);cursor=raw.end;}
 assert(chunks.length>0);
 const modules=new Map(),insert=/modules\.insert\(\s*"([^"]+)"\.to_string\(\),\s*/g;
 for(const match of source.matchAll(insert)){
  const tail=source.slice(match.index+match[0].length);
  if(/^(?:compressed_js_literal!\(\s*)?r#*"/.test(tail)){modules.set(match[1],rawLiteral(tail).value.trim());}
  else if(/^compressed_js_literal!\(crate::/.test(tail)){
   const constant=/^compressed_js_literal!\(crate::(\w+)::(\w+)\)/.exec(tail);
   if(constant){const owner=fs.readFileSync(PI+'/src/'+constant[1]+'.rs','utf8');modules.set(match[1],rawLiteral(owner,owner.indexOf('const '+constant[2])).value.trim());}
  }
 }
 const osStart=source.indexOf('fn build_node_os_module()'),osFormat=source.indexOf('format!(',osStart);
 let osSource=rawLiteral(source,osFormat).value;
 const values={node_platform:WINDOWS?'win32':'linux',node_arch:'x64',node_type:WINDOWS?'Windows_NT':'Linux',tmpdir:CWD+'/tmp',homedir:CWD+'/home',hostname:'fixture',eol:WINDOWS?'\r\n':'\n',dev_null:WINDOWS?'\\\\.\\NUL':'/dev/null',uid:1000,gid:1000,username:'reporter',shell:WINDOWS?'cmd.exe':'/bin/sh',num_cpus:2};
 for(const[name,value]of Object.entries(values))osSource=osSource.replaceAll('{'+name+'}',JSON.stringify(value));
 modules.set('node:os',osSource.replaceAll('{{','{').replaceAll('}}','}').trim());
 return{bridge:chunks.join(''),modules};
}
function createRuntime({baselineUndici=false,extensionId=EXTENSION}={}){
 const embedded=embeddedSources();if(baselineUndici)embedded.modules.delete('undici');
 const timers=new Map(),calls=[],canceled=[],diagnostics=[],moduleCache=new Map(),httpStreams=new Map(),execAttempts=[];
 let nextId=0,now=Date.now(),context;
 let respond=()=>({error:{code:'HTTP_FORBIDDEN',message:'Unscripted probe network denied'}});
 const env={HOME:CWD+'/home',USERPROFILE:CWD+'/home',TMPDIR:CWD+'/tmp',TEMP:CWD+'/tmp',PI_CODING_AGENT_DIR:CWD+'/home/.pi/agent',PI_FREEFLOW_CHECK_UPDATES:'0',PI_FREEFLOW_AUTO_UPDATE:'0'};
 function fixturePath(input){
  const value=String(input).replaceAll('\\','/'),m=WINDOWS?value.toLowerCase():value,p=WINDOWS?PKG.toLowerCase():PKG,c=WINDOWS?CWD.toLowerCase():CWD;
  if(m===p||m.startsWith(p+'/')){
   const local=path.resolve(UPSTREAM,value.slice(PKG.length).replace(/^\//,''));
   if(local!==UPSTREAM&&!local.startsWith(UPSTREAM+'/'))throw Error('fixture read denied');
      let real;
   try{real=fs.realpathSync(local);}catch(error){if(error.code!=='ENOENT')throw error;real=local;}
   if(real!==UPSTREAM&&!real.startsWith(UPSTREAM+path.sep))throw Error('fixture symlink read denied');
   return{local:real,value};
  }
  if(m===c||m.startsWith(c+'/'))return{local:null,value};
  throw Error('host read denied: path outside fixture root: '+value);
 }
 function deliver(id,outcome){context.__pi_complete_hostcall(SECRET,id,outcome);}
 function pumpHttpStream(id){
  const stream=httpStreams.get(String(id));
  if(!stream?.credit||!stream.frames?.length||stream.scheduled)return;
  stream.credit=false;stream.scheduled=true;
  const frame=stream.frames.shift();
  queueMicrotask(()=>{if(httpStreams.get(String(id))!==stream)return;stream.scheduled=false;stream.sequence=frame.sequence+1;deliver(String(id),frame);if(frame.isFinal)httpStreams.delete(String(id));else pumpHttpStream(id);});
 }
 function makeCall(kind,args){
  const id=String(++nextId),entry={id,kind,args:structuredClone(args)};calls.push(entry);
  if(kind==='http'&&args[0]?.stream)httpStreams.set(id,{credit:false,frames:null,scheduled:false});
  queueMicrotask(async()=>{try{
   const result=kind==='http'?await respond(entry):{value:null};
   if(result?.pending)return;
   if(result?.error){httpStreams.delete(id);deliver(id,{ok:false,...result.error});return;}
   const response=result?.value??result;
   if(kind==='http'&&args[0]?.stream){
    const stream=httpStreams.get(id);if(!stream)return;
    stream.frames=[{stream:true,sequence:0,chunk:{status:response.status??200,headers:response.headers??{}},isFinal:false}];
    let sequence=1;
    for(const body of response.chunks??[response.body??''])stream.frames.push({stream:true,sequence:sequence++,chunk:{body_bytes:Buffer.from(body).toString('base64')},isFinal:false});
    if(!response.open)stream.frames.push({stream:true,sequence,chunk:null,isFinal:true});
    pumpHttpStream(id);
   }else deliver(id,{ok:true,value:response});
  }catch(error){deliver(id,{ok:false,code:'PROBE_HANDLER',message:error.message});}});
  return id;
 }
 const natives={};
 for(const match of (embedded.bridge+'\n'+[...embedded.modules.values()].join('\n')).matchAll(/\b(__pi_[a-zA-Z0-9_]+_native)\b/g))natives[match[1]]=(...args)=>{throw Error('Unimplemented controlled binding '+match[1]+'('+args.map(String).join(',')+')');};
 Object.assign(natives,{
  __pi_bridge_secret_matches_native:candidate=>candidate===SECRET,__pi_owner_extension_id_native:()=>extensionId,
  __pi_process_cwd_native:()=>CWD,__pi_process_args_native:()=>['pi'],__pi_process_platform_native:()=>WINDOWS?'win32':'linux',
  __pi_process_execpath_native:()=>CWD+'/pi'+(WINDOWS?'.exe':''),__pi_env_get_native:key=>env[key]??null,__pi_now_ms_native:()=>now,
  __pi_url_parse_native:(input,base)=>{
   try{
    const url=base===null?new URL(input):new URL(input,base),fragment=url.href.indexOf('#'),query=url.href.indexOf('?');
    return JSON.stringify({href:url.href,protocol:url.protocol,hasAuthority:url.href.startsWith(url.protocol+'//'),hostname:url.hostname,host:url.host,port:url.port,username:url.username,password:url.password,pathname:url.pathname,search:query>=0&&(fragment<0||query<fragment)?url.href.slice(query,fragment<0?undefined:fragment):'',hash:fragment>=0?url.href.slice(fragment):'',origin:url.origin});
   }catch{return undefined;}
  },
  __pi_crypto_random_uuid_native:()=>crypto.randomUUID(),
  __pi_crypto_timing_safe_equal_native:(a,b)=>crypto.timingSafeEqual(Buffer.from(a),Buffer.from(b)),
  __pi_crypto_random_bytes_native:length=>Array.from(crypto.randomBytes(length)),
  __pi_crypto_hash_native:(algorithm,bytes,encoding)=>crypto.createHash(algorithm).update(Buffer.from(bytes)).digest(encoding),
  __pi_base64_encode_native:binary=>Buffer.from(binary,'latin1').toString('base64'),__pi_base64_encode_bytes_native:bytes=>Buffer.from(bytes).toString('base64'),
  __pi_base64_decode_native:base64=>Buffer.from(base64,'base64').toString('latin1'),__pi_console_output_native:(level,message)=>diagnostics.push({level,message}),
  __pi_set_timeout_native:delay=>{const id=++nextId;timers.set(id,{delay,at:now+delay});return id;},__pi_clear_timeout_native:id=>timers.delete(id),
  __pi_http_stream_pull_native:id=>{const stream=httpStreams.get(String(id));if(stream){stream.credit=true;pumpHttpStream(id);}},
  __pi_cancel_hostcall_native:id=>{id=String(id);canceled.push(id);const stream=httpStreams.get(id);httpStreams.delete(id);if(stream)queueMicrotask(()=>deliver(id,{stream:true,sequence:stream.sequence??0,chunk:null,isFinal:true}));return true;},
  __pi_http_native:(...args)=>makeCall('http',args),__pi_tool_native:(...args)=>makeCall('tool',args),__pi_session_native:(...args)=>makeCall('session',args),
  __pi_ui_native:(...args)=>makeCall('ui',args),__pi_events_native:(...args)=>makeCall('events',args),__pi_log_native:(...args)=>makeCall('log',args),
  __pi_exec_native:(...args)=>{execAttempts.push(args);throw Error('Process execution denied in controlled fixture');},__pi_exec_sync_native:(...args)=>{execAttempts.push(args);throw Error('Process execution denied in controlled fixture');},
  __pi_warn_unknown_event_native:()=>{},__pi_host_check_read_access:input=>{fixturePath(input);},__pi_host_check_write_access:input=>{fixturePath(input);},
  __pi_host_temp_roots_json:()=>JSON.stringify([CWD+'/tmp']),
  __pi_host_read_file_sync:input=>{const mapped=fixturePath(input);if(!mapped.local)throw Error('ENOENT: fixture file missing');return fs.readFileSync(mapped.local).toString('base64');},
  __pi_host_stat_sync:input=>{
   const mapped=fixturePath(input);
   if(mapped.local){const stat=fs.statSync(mapped.local);return JSON.stringify({kind:stat.isDirectory()?'dir':'file',size:stat.size});}
   if([CWD,CWD+'/home',CWD+'/tmp'].includes(mapped.value))return JSON.stringify({kind:'dir',size:0});
   throw Error('ENOENT: fixture path missing');
  },
 });
 context=vm.createContext({...natives});
 const getModule=(specifier,referring='')=>{
  let id=specifier;
  if(specifier.startsWith('.'))id=new URL(specifier,referring).href;
  else if(specifier.startsWith('/'))id='file://'+specifier;
  else if(!specifier.startsWith('file:')&&!embedded.modules.has(specifier)&&embedded.modules.has('node:'+specifier))id='node:'+specifier;
  if(moduleCache.has(id))return moduleCache.get(id);
  let text=embedded.modules.get(id);
  if(text===undefined){
   if(!id.startsWith('file:'))throw Error('Package module specifiers are not supported in PiJS: '+specifier);
   let virtualPath=decodeURIComponent(new URL(id).pathname);if(/^\/[a-z]:\//i.test(virtualPath))virtualPath=virtualPath.slice(1);
   const mapped=fixturePath(virtualPath);if(!mapped.local)throw Error('Module unavailable in fixture: '+id);
   // Node 26 accepts only mode 'strip'; the pinned fixture uses no syntax that needs 'transform'.
   text=stripTypeScriptTypes(fs.readFileSync(mapped.local,'utf8'),{mode:'strip'});
  }
  const module=new vm.SourceTextModule(text,{context,identifier:id,initializeImportMeta:meta=>{meta.url=id;},importModuleDynamically:async(name,owner)=>{const imported=getModule(name,owner.identifier);if(imported.status==='unlinked')await imported.link(linker);if(imported.status==='linked')await imported.evaluate();return imported;}});
  moduleCache.set(id,module);return module;
 };
 const linker=(name,owner)=>getModule(name,owner.identifier);
 vm.runInContext(embedded.bridge,context,{filename:'exact-pijs-bootstrap.js',timeout:3000});
 context.__pi_begin_extension(SECRET,extensionId,{name:'pi-freeflow',version:'1.39.0'});
 return{context,calls,canceled,diagnostics,timers,moduleCache,execAttempts,setResponder:fn=>{respond=fn;},
  failHttpStream(id,code,message){
   id=String(id);
   if(!httpStreams.delete(id))throw Error('No active controlled HTTP stream: '+id);
   deliver(id,{ok:false,code,message});
  },
  activeHttpStreamCount:()=>httpStreams.size,
  async module(specifier){const module=getModule(specifier);if(module.status==='unlinked')await module.link(linker);if(module.status==='linked')await module.evaluate({timeout:3000});return module.namespace;},
  async load(relative){const root='file://'+(WINDOWS?'/':'')+PKG+'/'+relative,module=getModule(root);if(module.status==='unlinked')await module.link(linker);if(module.status==='linked')await module.evaluate({timeout:3000});return module.namespace;},
  async advance(ms){now+=ms;for(const[id,timer]of[...timers])if(timer.at<=now){timers.delete(id);context.__pi_fire_timer(SECRET,id);}await flush();},
 };
}
async function flush(){for(let i=0;i<25;i++)await Promise.resolve();}
async function bounded(promise){let timer;try{return await Promise.race([promise,new Promise((_,reject)=>{timer=setTimeout(()=>reject(Error('Probe operation did not settle in 3 seconds')),3000);})]);}finally{clearTimeout(timer);}}
async function main(){
 assert.equal(JSON.parse(fs.readFileSync(UPSTREAM+'/package.json','utf8')).version,'1.39.0');
 const baseline=createRuntime({baselineUndici:true});
 await assert.rejects(baseline.load('src/relay.ts'),/Package module specifiers are not supported in PiJS: undici/);
 console.log('PASS reported undici import error reproduced with exact upstream relay.ts and absent registration');
 const r=createRuntime(),relay=await r.load('src/relay.ts');assert.equal(relay.canUseRelayDispatcher,false);
 const entry=await r.load('extensions/index.ts');let ready=false,failure;
 entry.default(r.context.pi).then(()=>{ready=true;},error=>{ready=true;failure=error;});
 for(let i=0;i<120&&!ready;i++)await r.advance(100);
 if(failure)throw failure;assert.equal(ready,true);
 const extension=r.context.__pi_snapshot_extensions(SECRET)[0],provider=extension.providers.find(p=>p.id==='freeflow');
 assert.equal(provider.models.length,35);assert.equal(typeof provider.virtualHttpServer,'string');
 assert.equal(extension.slash_commands.some(command=>command.name==='freeflow'),true);
 console.log('PASS exact upstream extension activation, /freeflow command, 35 models and owned virtual provider transport ('+(WINDOWS?'Windows':'POSIX')+' path projection)');
 assert.equal(provider.compat.supportsDeveloperRole,false);
 const catJam=provider.models.find(model=>model.id==='cat-jam-free');
 assert.equal(catJam.compat.supportsReasoningEffort,true);assert.equal(catJam.thinkingLevelMap.max,'max');assert.equal(catJam.thinkingLevelMap.minimal,null);
 const dots=provider.models.find(model=>model.id==='dots-studio/dots-3-note-preview:free');
 assert.equal(dots.compat.thinkingFormat,'openrouter');assert.equal(dots.thinkingLevelMap.max,null);
 const ovh=provider.models.find(model=>model.id==='ovh-free/gpt-oss-120b');
 assert.equal(ovh.compat.supportsReasoningEffort,true);assert.equal(ovh.thinkingLevelMap.low,'low');assert.equal(ovh.thinkingLevelMap.medium,null);
 console.log('PASS exact upstream provider compatibility and model-specific reasoning contracts');
 const origin=new URL(provider.baseUrl).origin;
 const health=await bounded(r.context.fetch(origin+'/health'));
 assert.equal(health.status,200,'the active proxy must answer its unauthenticated identity probe');
 assert.deepEqual(JSON.parse(await health.text()),{ok:true,service:'pi-freeflow',port:28180,version:'1.39.0'});
 const denied=await bounded(r.context.fetch(origin+'/_health'));
 assert.equal(denied.status,401);await denied.text();
 const headers={authorization:'Bearer '+provider.apiKey,'content-type':'application/json'};
 for(const action of ['attach','heartbeat','detach']){
  const response=await bounded(r.context.fetch(origin+'/_client/'+action,{method:'POST',headers,body:JSON.stringify({id:'compat-probe-client'})}));
  assert.equal(response.status,200,'authenticated client '+action+' must reach its control handler');
  assert.equal((await response.json()).ok,true);
 }
 const client=await r.load('src/client.ts');
 assert.equal(await bounded(client.ensureDaemon()),28180,'a healthy proxy must be reused without walking to another port');
 assert.equal(r.context.__pi_runtime_registry_snapshot(SECRET).virtualHttpServers,1);
 console.log('PASS actual proxy identity, authenticated lease controls and healthy same-port reuse');
}
export{createRuntime,flush,bounded,PI,UPSTREAM,PKG,CWD,SECRET};

await main();
{

const runtime=createRuntime(),relay=await runtime.load('src/relay.ts'),state=await runtime.load('src/relay-state.ts');
const relayA='https://relay-a.example',upstream='https://api.example/v1/chat/completions';
state.setActiveRelayState({...state.resolveRelayState(),enabled:true,url:relayA,relays:[{url:relayA,auth:'relay-secret'}]},false);
runtime.setResponder(()=>({status:200,headers:{'content-type':'text/event-stream'},chunks:[],open:true}));
const headers=new runtime.context.Headers([['Authorization','Bearer upstream-secret'],['content-type','application/json']]);
const result=await bounded(relay.relayFetch(upstream,{method:'POST',headers,body:'{"model":"fixture"}'},'probe'));
assert.equal(result.status,200);
const req=runtime.calls.at(-1).args[0];
assert.equal(req.url,relayA);assert.equal(req.headers['x-relay-target'],'https://api.example');
assert.equal(req.headers['x-relay-path'],'/v1/chat/completions');assert.equal(req.headers['x-relay-auth'],'relay-secret');
assert.equal(req.headers.authorization,'Bearer upstream-secret');assert.equal(headers.get('x-relay-target'),null);
assert.equal(req.dispatcher,undefined);
await bounded(result.body.cancel());
assert.deepEqual(runtime.canceled,[runtime.calls.at(-1).id]);
console.log('PASS relay request headers, authentication, dispatcher fallback, early headers and unread body cancellation');
runtime.calls.length=0;runtime.canceled.length=0;
const ctl=new runtime.context.AbortController();
runtime.setResponder(()=>({pending:true}));
const pending=relay.relayFetch(upstream,{method:'POST',headers,signal:ctl.signal},'canceled');
await flush();ctl.abort();
await assert.rejects(bounded(pending),error=>error.name==='AbortError');
assert.equal(runtime.calls.length,1);
assert.deepEqual(runtime.canceled,[runtime.calls[0].id]);
console.log('PASS in-flight caller abort cancels one hostcall without direct fallback');

}
{

const upstream='https://api.example/v1/chat/completions?key=value',relayA='https://relay-a.example',relayB='https://relay-b.example';
async function configured(urls=[relayA]){
 const r=createRuntime(),relay=await r.load('src/relay.ts'),state=await r.load('src/relay-state.ts');
 state.setActiveRelayState({...state.resolveRelayState(),enabled:true,url:urls[0],mode:'sticky',relays:urls.map(url=>({url,auth:'relay-secret'}))},true);
 return{r,relay,state};
}
{
 const{r,relay}=await configured();const ctl=new r.context.AbortController();ctl.abort();
 await assert.rejects(bounded(relay.relayFetch(upstream,{signal:ctl.signal},'preabort')),e=>e.name==='AbortError');
 assert.equal(r.calls.length,0);console.log('PASS already-aborted caller creates zero HTTP hostcalls');
}
{
 const{r,relay}=await configured();
 r.setResponder(entry=>entry.args[0].url===relayA?{pending:true}:{status:200,chunks:[],open:true});
 const pending=relay.relayFetch(upstream,{method:'POST',headers:{Authorization:'Bearer upstream'},body:'original-body'},'timeout');
 await flush();assert.equal(r.calls.length,1);await r.advance(300000);
 const response=await bounded(pending);assert.equal(response.status,200);assert.equal(r.calls.length,2);
 assert.equal(r.calls[1].args[0].url,upstream);assert.equal(r.calls[1].args[0].body,'original-body');
 assert.equal(r.calls[1].args[0].headers['x-relay-target'],undefined);
 assert.equal(r.calls[1].args[0].headers['x-relay-auth'],undefined);
 assert.equal(r.calls[1].args[0].headers.authorization,'Bearer upstream');
 assert.deepEqual(r.canceled,[r.calls[0].id]);await response.body.cancel();
 console.log('PASS header timeout cancels hung relay, direct fallback strips relay-only headers and retains request/auth');
}
{
 const{r,relay,state}=await configured([relayA,relayB]);
 r.setResponder(entry=>entry.args[0].url===relayA?{pending:true}:{status:200,chunks:[],open:true});
 const pending=relay.relayFetch(upstream,{method:'POST',body:'hedged'},'hedge');
 await flush();assert.equal(r.calls.length,1);await r.advance(8000);
 const response=await bounded(pending);assert.equal(response.status,200);
 assert.deepEqual(r.calls.map(c=>c.args[0].url),[relayA,relayB]);assert.deepEqual(r.canceled,[r.calls[0].id]);
 assert.equal(state.isRelayHealthy(relayA),true);
 await response.body.cancel();
 console.log('PASS hedged failover aborts loser, preserves healthy status and returns winning headers');
}
{
 const{r,relay}=await configured();
 r.setResponder(()=>({error:{code:'HTTP_FORBIDDEN',message:'Denied by host capability policy'}}));
 await assert.rejects(bounded(relay.relayFetch(upstream,{},'denied')),e=>e.code==='HTTP_FORBIDDEN');
 assert.equal(r.calls.length,2);console.log('PASS controlled capability rejection remains an error across relay and direct fallback');
}

}
{

const runtime=createRuntime();
const relay=await runtime.load('src/relay.ts'),state=await runtime.load('src/relay-state.ts');
assert.equal(relay.canUseRelayDispatcher,false);
const upstream='https://api.example/v1/chat/completions?trace=1';
const relayA='https://relay-a.example',relayB='https://relay-b.example';
const headers=new runtime.context.Headers([['Authorization','Bearer test-upstream'],['Content-Type','application/json']]);
const requestBody=JSON.stringify({model:'fixture-model',messages:[{role:'user',content:'hello'}],stream:true});
state.setActiveRelayState({...state.resolveRelayState(),enabled:false,relays:[]},false);
runtime.setResponder(()=>({status:200,headers:{'content-type':'text/event-stream'},chunks:['data: {"text":"hé','llo"}\n\n','data: [DONE]\n\n']}));
const direct=await bounded(relay.relayFetch(upstream,{method:'POST',headers,body:requestBody},'direct-1'));
assert.equal(direct.status,200);
assert.equal(await bounded(direct.text()),'data: {"text":"héllo"}\n\ndata: [DONE]\n\n');
assert.equal(runtime.calls[0].args[0].url,upstream);
assert.equal(runtime.calls[0].args[0].body,requestBody);
console.log('PASS direct disabled relay with streamed UTF-8 body and request body');
runtime.calls.length=0;
state.setActiveRelayState({...state.resolveRelayState(),enabled:true,url:relayA,mode:'sticky',relays:[{url:relayA,auth:'relay-secret'}]},false);
const result=await bounded(relay.relayFetch(upstream,{method:'POST',headers,body:requestBody},'relay-1'));
assert.equal(await bounded(result.text()),'data: {"text":"héllo"}\n\ndata: [DONE]\n\n');
assert.equal(runtime.calls.length,1);
const sent=runtime.calls[0].args[0];
assert.equal(sent.url,relayA);
assert.equal(sent.headers['x-relay-target'],'https://api.example');
assert.equal(sent.headers['x-relay-path'],'/v1/chat/completions?trace=1');
assert.equal(sent.headers['x-relay-auth'],'relay-secret');
assert.equal(sent.headers.authorization,'Bearer test-upstream');
assert.equal(headers.get('x-relay-target'),null);
assert.equal(sent.dispatcher,undefined);
console.log('PASS real relay operation, auth/target/path headers, original headers intact, no dispatcher');

}
{

{
 const r=createRuntime();
 const bytes=Buffer.from('π🙂héllo!');
 r.setResponder(()=>({status:200,headers:{'Content-Type':'text/plain'},chunks:[bytes.subarray(0,1),bytes.subarray(1,4),bytes.subarray(4,7),bytes.subarray(7)]}));
 const response=await bounded(r.context.fetch('https://controlled.invalid/text'));
 const clone=response.clone();
 assert.deepEqual(await bounded(Promise.all([response.text(),clone.text()])),['π🙂héllo!','π🙂héllo!']);
 console.log('PASS clone streams concurrently across split UTF8 bytes and four chunks');
}
{
 const r=createRuntime();
 r.setResponder(()=>({status:200,headers:{},chunks:['pending'],open:true}));
 const response=await bounded(r.context.fetch('https://controlled.invalid/open'));
 const clone=response.clone(),cancelClone=clone.body.cancel('unused branch');
 await flush();assert.equal(r.canceled.length,0);
 await bounded(response.body.cancel('caller canceled'));await bounded(cancelClone);
 assert.equal(r.canceled.length,1);
 console.log('PASS unused clone cancellation preserves other consumer, both cancellation closes host exactly once');
}
{
 const r=createRuntime(),controller=new r.context.AbortController();
 r.setResponder(()=>({pending:true}));
 const timeout=r.context.AbortSignal.timeout(25),combined=r.context.AbortSignal.any([controller.signal,timeout]);
 const result=r.context.fetch('https://controlled.invalid/timeout',{signal:combined}).then(()=>({ok:true}),error=>({error}));
 await flush();await r.advance(25);
 const settled=await bounded(result);
 assert.equal(settled.error?.name,'TimeoutError');assert.equal(controller.signal.aborted,false);assert.equal(r.canceled.length,1);
 console.log('PASS timeout abort reason reaches fetch and cancels host without aborting caller signal');
}
{
 const r=createRuntime(),controller=new r.context.AbortController(),combined=r.context.AbortSignal.any([controller.signal]);
 controller.signal.dispatchEvent({type:'abort'});
 assert.equal(controller.signal.aborted,false);assert.equal(combined.aborted,false);
 const reason={fixture:'first'},second={fixture:'second'};
 controller.abort(reason);controller.abort(second);
 assert.equal(controller.signal.reason,reason);assert.equal(combined.reason,reason);
 console.log('PASS synthetic abort event cannot forge AbortSignal.any abort and first reason remains');
}
{
 const r=createRuntime();r.setResponder(()=>({error:{code:'HTTP_FORBIDDEN',message:'Controlled policy denial'}}));
 const error=await r.context.fetch('https://controlled.invalid/denied').then(()=>null,e=>e);
 assert.equal(error.code,'HTTP_FORBIDDEN');assert.match(error.message,/Controlled policy denial/);
 console.log('PASS fetch retains capability-denial error code');
}

{
 const r=createRuntime(),controller=new r.context.AbortController(),reason=new Error('genuine abort after synthetic event');
 r.setResponder(()=>({pending:true}));
 const result=r.context.fetch('https://controlled.invalid/hung',{signal:controller.signal}).then(()=>({ok:true}),error=>({error}));
 await flush();controller.signal.dispatchEvent({type:'abort'});await flush();
 assert.equal(r.canceled.length,0);assert.equal(controller.signal.aborted,false);
 controller.abort(reason);
 assert.equal((await bounded(result)).error,reason);assert.equal(r.canceled.length,1);
 console.log('PASS synthetic fetch abort event cannot remove the later real cancellation listener');
}
{
 const r=createRuntime();
 let signal,combined;
 r.context.pi.registerProvider('signal-fixture',{api:'fixture',models:[],streamSimple:(_model,_context,options)=>{
  signal=options.signal;combined=r.context.AbortSignal.any([options.signal]);
  return {next:async()=>({done:false,value:{ready:true}}),return:async()=>({done:true}),[Symbol.asyncIterator](){return this;}};
 }});
 const id=await r.context.__pi_provider_stream_simple_start(SECRET,'signal-fixture',{}, {}, {});
 assert.equal(signal.aborted,false);assert.equal(combined.aborted,false);
 assert.equal((await r.context.__pi_provider_stream_simple_next(SECRET,id)).value.ready,true);
 assert.equal(await r.context.__pi_provider_stream_simple_cancel(SECRET,id),true);
 assert.equal(signal.aborted,true);assert.equal(combined.aborted,true);assert.equal(combined.reason,signal.reason);
 console.log('PASS provider streamSimple receives branded signal usable with AbortSignal.any and cancellation');
}

}
{

const virtualTests=fs.readFileSync(PI+'/tests/virtual_http_server.rs','utf8');
for(const[name,expected]of [
 ['virtual_http_preserves_request_routing_headers_and_binary_bytes',{method:'POST',path:'/v1/chat/completions?q=1',token:'retained',host:'127.0.0.1:28180',status:201,header:'yes',bytes:[0,255,65]}],
 ['virtual_http_fetch_settles_at_headers_before_delayed_body',{beforeBody:false,body:'data: hello\n\n',ended:true}],
 ['virtual_http_abort_before_headers_closes_the_actual_request_once',{name:'AbortError',closed:1}],
 ['virtual_http_body_cancel_releases_the_producer',{text:'first',closed:true}],
 ['virtual_http_rejects_external_bind_and_bounds_unconsumed_response',{externalDenied:true,bounded:true}],
]){
 const r=createRuntime(),start=virtualTests.indexOf('fn '+name+'('),body=rawLiteral(virtualTests,start).value;
 r.context.http=(await r.module('node:http')).default;
 const result=await bounded(vm.runInContext('(async()=>{'+body+'})()',r.context));
 assert.deepEqual(JSON.parse(JSON.stringify(result.result)),expected);
 await flush();assert.equal(r.calls.length,0);assert.equal(r.context.__pi_runtime_registry_snapshot(SECRET).virtualHttpRequests,0);
 console.log('PASS exact Rust test JS body: '+name);
}
{
 const a=createRuntime({extensionId:'extension-A'}),b=createRuntime({extensionId:'extension-B'});
 for(const r of [a,b])r.context.http=(await r.module('node:http')).default;
 vm.runInContext("globalThis.server=http.createServer((_req,res)=>res.end('private-A'));server.listen(28180,'127.0.0.1')",a.context);
 await flush();
 const error=await b.context.fetch('http://127.0.0.1:28180/private').then(()=>null,e=>e);
 assert.equal(error.code,'HTTP_FORBIDDEN');assert.equal(a.calls.length,0);assert.equal(b.calls.length,1);
 vm.runInContext("globalThis.server=http.createServer((_req,res)=>res.end('private-B'));server.listen(28180,'127.0.0.1')",b.context);
 await flush();
 assert.equal(await (await a.context.fetch('http://127.0.0.1:28180/private')).text(),'private-A');
 assert.equal(await (await b.context.fetch('http://127.0.0.1:28180/private')).text(),'private-B');
 assert.throws(()=>a.context.__pi_virtual_http_start('incorrect-secret','forged',{}),/secret|privileged|bridge|auth/i);
 a.context.server.close();b.context.server.close();
 console.log('PASS independent extension realms isolate identical virtual loopback ports and require host bridge secret');
}
{
 const r=createRuntime(),http=(await r.module('node:http')).default;
 const server=http.createServer((_req,res)=>res.end('owned'));server.listen(28180,'127.0.0.1');await flush();
 const model={id:'fixture',name:'Fixture',contextWindow:4096,maxTokens:512};
 r.context.pi.registerProvider('owned-fixture',{api:'openai-completions',baseUrl:'http://127.0.0.1:28180/v1',models:[model]});
 r.context.pi.registerProvider('forged-fixture',{api:'openai-completions',baseUrl:'https://example.com/v1',models:[model],virtualHttpServer:'virtual-http-server-1'});
 const providers=r.context.__pi_snapshot_extensions(SECRET)[0].providers;
 assert.equal(typeof providers.find(p=>p.id==='owned-fixture').virtualHttpServer,'string');
 assert.equal(providers.find(p=>p.id==='forged-fixture').virtualHttpServer,undefined);
 server.close();
 console.log('PASS provider transport derives only from owned listener and cannot be forged through metadata');
}

}
{

const runtime=createRuntime(),entry=await runtime.load('extensions/index.ts');
let ready=false,failure;
entry.default(runtime.context.pi).then(()=>{ready=true;},e=>{ready=true;failure=e;});
for(let i=0;i<120&&!ready;i++)await runtime.advance(100);
if(failure)throw failure;assert.equal(ready,true);
const provider=runtime.context.__pi_snapshot_extensions(SECRET)[0].providers.find(p=>p.id==='freeflow');
const origin=new URL(provider.baseUrl).origin;
const stopped=await bounded(runtime.context.fetch(origin+'/_shutdown',{method:'POST',headers:{authorization:'Bearer '+provider.apiKey}}));
assert.equal(stopped.status,200);assert.equal((await stopped.json()).ok,true);
await runtime.advance(50);
let reattached=false,reattachError;
runtime.context.__pi_dispatch_extension_event(SECRET,'session_start',{type:'session_start'},{hasUI:false,model:{provider:'freeflow',id:'ovh-free/gpt-oss-20b'}}).then(()=>{reattached=true;},error=>{reattachError=error;reattached=true;});
for(let i=0;i<120&&!reattached;i++)await runtime.advance(100);
if(reattachError)throw reattachError;assert.equal(reattached,true,'the actual session lifecycle must finish recovering its proxy');
const health=await bounded(runtime.context.fetch(origin+'/health'));
assert.equal(health.status,200);assert.equal((await health.json()).port,28180);
const recovered=runtime.context.__pi_snapshot_extensions(SECRET)[0].providers.find(p=>p.id==='freeflow');
assert.equal(recovered.virtualHttpServer,provider.virtualHttpServer,'same-port upstream recovery does not re-register its provider');
assert.equal(runtime.context.__pi_runtime_registry_snapshot(SECRET).virtualHttpServers,1);
const streamText='data: '+JSON.stringify({id:'chatcmpl-fixture',object:'chat.completion.chunk',model:'gpt-oss-20b',choices:[{index:0,delta:{content:'Hello π🙂héllo from actual proxy'},finish_reason:null}]})+'\n\n'+'data: '+JSON.stringify({id:'chatcmpl-fixture',object:'chat.completion.chunk',model:'gpt-oss-20b',choices:[{index:0,delta:{},finish_reason:'stop'}]})+'\n\n'+'data: [DONE]\n\n';
runtime.calls.length=0;
runtime.setResponder(entry=>{
 const request=entry.args[0];
 if(request.url==='https://oai.endpoints.kepler.ai.cloud.ovh.net/v1/chat/completions')return{status:200,headers:{'content-type':'text/event-stream'},chunks:(()=>{const bytes=Buffer.from(streamText),cut=bytes.indexOf(Buffer.from('π'))+1;return [bytes.subarray(0,cut),bytes.subarray(cut,cut+3),bytes.subarray(cut+3,cut+5),bytes.subarray(cut+5)];})()};
 return{error:{code:'HTTP_FORBIDDEN',message:'Unscripted host network denied'}};
});
const request={url:provider.baseUrl+'/chat/completions',method:'POST',headers:{authorization:'Bearer '+provider.apiKey,'content-type':'application/json'},body:JSON.stringify({model:'ovh-free/gpt-oss-20b',messages:[{role:'user',content:'hello'}],stream:true})};
const id=runtime.context.__pi_virtual_http_start(SECRET,'freeflow',request);
let status,body='',done=false;
for(let i=0;i<500;i++){
 await runtime.advance(0);
 const next=await runtime.context.__pi_provider_stream_simple_next(SECRET,id);
 if(next.done){done=true;break;}
 if(next.value?.status)status=next.value.status;
 if(next.value?.body_bytes)body+=Buffer.from(next.value.body_bytes,'base64').toString('utf8');
}

assert.equal(done,true);assert.equal(status,200);assert.match(body,/Hello π🙂héllo from actual proxy/);assert.match(body,/\[DONE\]/);
assert.equal(runtime.calls.length,1);
const outbound=runtime.calls[0].args[0];
const outboundBody=outbound.body??Buffer.from(outbound.body_bytes,'base64').toString();
assert.equal(JSON.parse(outboundBody).model,'gpt-oss-20b');
assert.equal(outbound.headers?.authorization,undefined);
console.log('PASS complete actual extension provider -> virtual local proxy -> capability HTTP mock -> streamed completion');
const counts=runtime.context.__pi_runtime_registry_snapshot(SECRET);
assert.equal(counts.virtualHttpRequests,0);assert.equal(counts.providerStreams,0);assert.equal(counts.pendingHostcalls,0);
(await runtime.load('src/client.ts')).stopHeartbeat();
console.log('PASS actual shutdown -> session-start recovery on the same port -> existing provider streamed completion');

}
{

const runtime=createRuntime(),entry=await runtime.load('extensions/index.ts');
let ready=false,failure;
entry.default(runtime.context.pi).then(()=>{ready=true;},e=>{ready=true;failure=e;});
for(let i=0;i<120&&!ready;i++)await runtime.advance(100);
if(failure)throw failure;assert.equal(ready,true);
const provider=runtime.context.__pi_snapshot_extensions(SECRET)[0].providers.find(p=>p.id==='freeflow');

runtime.calls.length=0;runtime.canceled.length=0;
runtime.setResponder(()=>({pending:true}));
const request={url:provider.baseUrl+'/chat/completions',method:'POST',headers:{authorization:'Bearer '+provider.apiKey,'content-type':'application/json'},body:JSON.stringify({model:'ovh-free/gpt-oss-20b',messages:[{role:'user',content:'hello'}],stream:true})};
const id=runtime.context.__pi_virtual_http_start(SECRET,'freeflow',request);
for(let i=0;i<30;i++)await runtime.advance(0);
assert.equal(runtime.calls.length,1);
assert.equal(runtime.canceled.length,0);
assert.equal(await runtime.context.__pi_provider_stream_simple_cancel(SECRET,id),true);
for(let i=0;i<30;i++)await runtime.advance(0);
assert.equal(runtime.canceled.length,1);
assert.equal(runtime.canceled[0],runtime.calls[0].id);
assert.equal((await runtime.context.__pi_provider_stream_simple_next(SECRET,id)).done,true);
const counts=runtime.context.__pi_runtime_registry_snapshot(SECRET);
assert.equal(counts.providerStreams,0);assert.equal(counts.virtualHttpRequests,0);assert.equal(counts.pendingHostcalls,0);
console.log('PASS actual provider cancellation before headers closes upstream host request and route');

}
{

const runtime=createRuntime(),entry=await runtime.load('extensions/index.ts');
let ready=false,failure;
entry.default(runtime.context.pi).then(()=>{ready=true;},e=>{ready=true;failure=e;});
for(let i=0;i<120&&!ready;i++)await runtime.advance(100);
if(failure)throw failure;assert.equal(ready,true);
const provider=runtime.context.__pi_snapshot_extensions(SECRET)[0].providers.find(p=>p.id==='freeflow');

runtime.calls.length=0;runtime.canceled.length=0;
const sse='data: {"id":"stream","choices":[{"index":0,"delta":{"content":"partial"}}]}\n\n';
runtime.setResponder(()=>({status:200,headers:{'content-type':'text/event-stream'},chunks:[sse],open:true}));
const request={url:provider.baseUrl+'/chat/completions',method:'POST',headers:{authorization:'Bearer '+provider.apiKey,'content-type':'application/json'},body:JSON.stringify({model:'ovh-free/gpt-oss-20b',messages:[{role:'user',content:'hello'}],stream:true})};
const id=runtime.context.__pi_virtual_http_start(SECRET,'freeflow',request);
let body='';
for(let i=0;i<100&&!body;i++){
 await runtime.advance(0);
 const next=await runtime.context.__pi_provider_stream_simple_next(SECRET,id);
 if(next.value?.body_bytes)body+=Buffer.from(next.value.body_bytes,'base64').toString();
}
assert.match(body,/partial/);
assert.equal(runtime.calls.length,1);
assert.equal(runtime.canceled.length,0);
assert.equal(await runtime.context.__pi_provider_stream_simple_cancel(SECRET,id),true);
for(let i=0;i<30;i++)await runtime.advance(0);
assert.equal(runtime.canceled.length,1);
assert.equal(runtime.canceled[0],runtime.calls[0].id);
assert.equal((await runtime.context.__pi_provider_stream_simple_next(SECRET,id)).done,true);
const counts=runtime.context.__pi_runtime_registry_snapshot(SECRET);
assert.equal(counts.providerStreams,0);assert.equal(counts.virtualHttpRequests,0);assert.equal(counts.pendingHostcalls,0);
console.log('PASS actual provider cancellation after streamed body closes upstream host request and route');

}
{
{
 const r=createRuntime(),{Readable}=await r.module('node:stream');
 let canceled=0,produced=0;
 const web=new r.context.ReadableStream({pull(controller){controller.enqueue(new Uint8Array([++produced]));},cancel(){canceled++;}});
 const stream=Readable.fromWeb(web),received=[];
 for await(const chunk of stream){received.push(chunk[0]);break;}
 await flush();assert.deepEqual(received,[1]);assert.equal(canceled,1);assert.equal(stream.destroyed,true);
 console.log('PASS Readable.fromWeb for-await break cancels the owned Web reader once');
}
{
 const r=createRuntime(),{Readable}=await r.module('node:stream');
 let produced=0;
 const web=new r.context.ReadableStream({pull(controller){if(produced===3)controller.close();else controller.enqueue(new Uint8Array([++produced]));}});
 const stream=Readable.fromWeb(web),received=[];
 const ended=new Promise((resolve,reject)=>{stream.on('data',chunk=>{received.push(chunk[0]);if(received.length===1)stream.pause();});stream.on('end',resolve);stream.on('error',reject);});
 await flush();assert.deepEqual(received,[1]);
 stream.resume();await bounded(ended);
 assert.deepEqual(received,[1,2,3]);assert.equal(stream._queue.length,0);
 console.log('PASS Readable.fromWeb pause/resume preserves byte order without retaining delivered chunks');
}
{
 const r=createRuntime(),http=(await r.module('node:http')).default,bytes=Buffer.from('π🙂héllo');
 r.setResponder(()=>({status:503,headers:{'content-type':'text/plain'},chunks:[bytes.subarray(0,1),bytes.subarray(1,4),bytes.subarray(4,7),bytes.subarray(7)]}));
 const result=await bounded(new Promise((resolve,reject)=>{
  const request=http.get('https://controlled.invalid/status',response=>{
   let text='';response.setEncoding('utf8');response.on('data',chunk=>{assert.equal(typeof chunk,'string');text+=chunk;});response.on('error',reject);response.on('end',()=>resolve({status:response.statusCode,text,complete:response.complete}));
  });request.on('error',reject);
 }));
 assert.deepEqual(result,{status:503,text:'π🙂héllo',complete:true});
 assert.equal(r.calls.length,1);assert.equal(r.calls[0].args[0].stream,true);
 console.log('PASS node:http preserves non-2xx response and decodes UTF8 split across streamed chunks');
}
{
 const r=createRuntime(),http=(await r.module('node:http')).default;r.setResponder(()=>({pending:true}));
 let aborted=0,closed=0,errors=0;
 const request=http.get('https://controlled.invalid/pending');
 request.on('abort',()=>aborted++);request.on('close',()=>closed++);request.on('error',()=>errors++);
 await flush();request.abort();request.abort();await flush();
 assert.equal(aborted,1);assert.equal(closed,1);assert.equal(errors,0);assert.equal(r.canceled.length,1);
 console.log('PASS node:http abort before headers closes one host request and emits events once');
}
{
 const r=createRuntime(),http=(await r.module('node:http')).default;
 r.setResponder(()=>({status:200,headers:{},chunks:['partial'],open:true}));
 let timeout=0,error,closed=0;
 const request=http.get('https://controlled.invalid/idle',response=>{response.on('error',()=>{});response.on('data',()=>{});});
 request.on('close',()=>closed++);request.on('error',value=>{error=value;});request.setTimeout(17,()=>timeout++);
 await flush();await r.advance(17);await flush();
 assert.equal(timeout,1);assert.equal(error.code,'ETIMEDOUT');assert.equal(closed,1);assert.equal(r.canceled.length,1);
 console.log('PASS node:http body inactivity timeout aborts live HTTP transport once');
}
}
{
{
 const r=createRuntime(),{spawn}=await r.module('node:child_process');
 assert.throws(()=>spawn('C:\\Program Files\\nodejs\\node.exe',['C:\\Users\\reporter\\freeflow\\daemon.ts'],{detached:true,stdio:'ignore',windowsHide:true,env:{PI_FREEFLOW_DAEMON:'0'}}),/unsupported option/);
 assert.equal(r.execAttempts.length,0);assert.equal(r.calls.length,0);
 console.log('PASS Windows detached-daemon options reject before any execution hostcall');
}
{
 const r=createRuntime(),https=(await r.module('node:https')).default;
 const errors=await bounded(Promise.all([{rejectUnauthorized:false},{ca:'custom'},{createConnection(){}},{socketPath:'/tmp/private.sock'}].map(options=>new Promise(resolve=>{
  const request=https.request('https://controlled.invalid/',options);
  request.on('error',error=>resolve(error.message));request.end();
 }))));
 assert.equal(errors.length,4);for(const message of errors)assert.match(message,/host networking/);
 assert.equal(r.calls.length,0);
 console.log('PASS custom TLS and socket options fail before any HTTP hostcall');
}
}
{
 const runtime=createRuntime(),entry=await runtime.load('extensions/index.ts');
 let ready=false,failure;
 entry.default(runtime.context.pi).then(()=>{ready=true;},error=>{ready=true;failure=error;});
 for(let i=0;i<120&&!ready;i++)await runtime.advance(100);
 if(failure)throw failure;assert.equal(ready,true);
 const provider=runtime.context.__pi_snapshot_extensions(SECRET)[0].providers.find(p=>p.id==='freeflow');
 const relayState=await runtime.load('src/relay-state.ts');
 const relayA='https://relay-a.example',relayB='https://relay-b.example';
 relayState.setActiveRelayState({...relayState.resolveRelayState(),enabled:true,url:relayA,mode:'sticky',relays:[{url:relayA},{url:relayB}]},true);
 const frame=text=>'data: '+JSON.stringify({id:'fixture',object:'chat.completion.chunk',choices:[{index:0,delta:{content:text},finish_reason:null}]})+'\n\n';
 const terminal='data: '+JSON.stringify({id:'fixture',object:'chat.completion.chunk',choices:[{index:0,delta:{},finish_reason:'stop'}]})+'\n\ndata: [DONE]\n\n';
 runtime.calls.length=0;
 runtime.setResponder(entry=>{
  const url=entry.args[0].url;
  if(url===relayA)return{status:200,headers:{'content-type':'text/event-stream'},chunks:[frame('first-prefix')],open:true};
  if(url===relayB)return{status:200,headers:{'content-type':'text/event-stream'},chunks:[frame('resumed-tail'),terminal]};
  return{error:{code:'HTTP_FORBIDDEN',message:'Unscripted host network denied'}};
 });
 const request={url:provider.baseUrl+'/chat/completions',method:'POST',headers:{authorization:'Bearer '+provider.apiKey,'content-type':'application/json'},body:JSON.stringify({model:'cat-jam-free',messages:[{role:'user',content:'hello'}],stream:true})};
 const id=runtime.context.__pi_virtual_http_start(SECRET,'freeflow',request);
 let body='',status;
 for(let i=0;i<100&&!body.includes('first-prefix');i++){
  await runtime.advance(0);
  const next=await runtime.context.__pi_provider_stream_simple_next(SECRET,id);
  if(next.value?.status)status=next.value.status;
  if(next.value?.body_bytes)body+=Buffer.from(next.value.body_bytes,'base64').toString();
 }
 assert.equal(status,200);assert.match(body,/first-prefix/);assert.equal(runtime.calls.length,1);
 assert.equal(runtime.activeHttpStreamCount(),1);
 // Model the native transport's terminal error after headers and one body
 // chunk. Its bookkeeping closes before the real bridge receives the error.
 runtime.failHttpStream(runtime.calls[0].id,'HTTP_IO','Controlled mid-body disconnect');
 let done=false;
 for(let i=0;i<300;i++){
  await runtime.advance(0);
  const next=await runtime.context.__pi_provider_stream_simple_next(SECRET,id);
  if(next.done){done=true;break;}
  if(next.value?.body_bytes)body+=Buffer.from(next.value.body_bytes,'base64').toString();
 }
 assert.equal(done,true);assert.match(body,/resumed-tail/);assert.match(body,/\[DONE\]/);
 assert.deepEqual(runtime.calls.map(call=>call.args[0].url),[relayA,relayB]);
 const counts=runtime.context.__pi_runtime_registry_snapshot(SECRET);
 assert.equal(counts.pendingHostcalls,0);assert.equal(counts.providerStreams,0);assert.equal(counts.virtualHttpRequests,0);
 assert.equal(runtime.activeHttpStreamCount(),0);
 console.log('PASS actual Freeflow post-header relay failure refires once, preserves streamed text and releases transports and bridge registries');
}
console.log('ALL supplementary exact-source JS probes passed; DSR/Rust/QuickJS remain separately required.');
