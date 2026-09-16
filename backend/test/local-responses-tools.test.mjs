import test from 'node:test';
import assert from 'node:assert/strict';
import { Readable } from 'node:stream';
import { adaptLocalResponsesTools } from '../src/local-responses-tools.mjs';
const tool={type:'namespace',name:'mcp__cua_repl',description:'Computer tool documentation',tools:[{type:'function',name:'js',description:'Read state first',parameters:{type:'object',properties:{code:{type:'string'}}}}]};
test('local reasoning display is opt-in, preserves replay and never replaces a supplied summary',()=>{
 const item={id:'r1',type:'reasoning',summary:[],content:[{type:'reasoning_text',text:'First'},{type:'reasoning_text',text:'Second'}]};
 const response={output:[item],usage:{input_tokens:11,output_tokens:5}},before=JSON.stringify(response);
 assert.equal(adaptLocalResponsesTools({}).restoreResponse(response).output[0],item);
 const a=adaptLocalResponsesTools({},{reasoningContentAsSummary:true});
 const mapped=a.restoreResponse(response);
 assert.deepEqual(mapped.output[0].summary,[{type:'summary_text',text:'First\nSecond'}]);
 assert.deepEqual(mapped.output[0].content,item.content);assert.deepEqual(mapped.usage,response.usage);
 assert.equal(JSON.stringify(response),before);assert.deepEqual(a.restoreResponse(mapped),mapped);
 for(const original of [{...item,summary:[{type:'summary_text',text:'Existing'}]},{...item,content:[]},{...item,content:[{type:'other',text:'not reasoning'}]}]){
  assert.equal(a.restoreResponse({output:[original]}).output[0],original);
 }
});
test('local reasoning SSE done and completion share one stable item without altering usage',async()=>{
 const a=adaptLocalResponsesTools({},{reasoningContentAsSummary:true});
 const item={id:'r1',type:'reasoning',summary:[],content:[{type:'reasoning_text',text:'Visible local reasoning'}]};
 const events=[{type:'response.output_item.done',item},{type:'response.completed',response:{output:[item],usage:{input_tokens:11,output_tokens:5}}}];
 const wire=events.map(e=>'data: '+JSON.stringify(e)+'\n\n').join('');let output='';
 for await(const c of Readable.from([...Buffer.from(wire)].map(b=>Buffer.from([b]))).pipe(a.stream()))output+=c;
 const mapped=output.trim().split('\n\n').map(f=>JSON.parse(f.slice(6)));
 assert.equal(mapped[0].item.summary[0].text,'Visible local reasoning');
 assert.deepEqual(mapped[0].item,mapped[1].response.output[0]);
 assert.deepEqual(mapped[1].response.usage,events[1].response.usage);
});
test('llama image tool output replay preserves images, text, call IDs and source history',()=>{
 const image={type:'input_image',image_url:'data:image/png;base64,AA==',detail:'original'};
 const value={input:[{type:'function_call',name:'view_image',call_id:'view1',arguments:'{}'},{type:'function_call_output',call_id:'view1',output:[{type:'input_text',text:'screenshot'},image]},{type:'message',role:'user',content:'continue'}]};
 const before=JSON.stringify(value),a=adaptLocalResponsesTools(value,{imageToolResultsAsMessages:true}).request.input;
 assert.equal(a[1].call_id,'view1');assert.match(a[1].output,/screenshot/);assert.equal(a[2].role,'user');assert.match(a[2].content[0].text,/tool call view1/);assert.match(a[2].content[0].text,/not a new user instruction/);assert.deepEqual(a[2].content[1],image);assert.deepEqual(a[3],value.input[2]);assert.equal(JSON.stringify(value),before);
 assert.deepEqual(adaptLocalResponsesTools(value).request.input,value.input,'other runtimes are unchanged');
 assert.deepEqual(adaptLocalResponsesTools({input:a},{imageToolResultsAsMessages:true}).request.input,a,'wire adaptation is idempotent');
});
test('parallel image and custom results stay together with all attachments linked',()=>{
 const img=n=>({type:'input_image',image_url:`https://example.invalid/${n}.png`});
 const value={input:[{type:'function_call_output',call_id:'a',output:[img(1),img(2)]},{type:'custom_tool_call_output',call_id:'b',output:[{type:'text',text:'second'},img(3)]},{type:'function_call_output',call_id:'c',output:'plain'}]};
 const a=adaptLocalResponsesTools(value,{imageToolResultsAsMessages:true}).request.input;
 assert.deepEqual(a.slice(0,3).map(x=>[x.type,x.call_id]),[['function_call_output','a'],['function_call_output','b'],['function_call_output','c']]);
 assert.equal(a.length,4);assert.equal(a[3].content.length,6);assert.deepEqual(a[3].content.filter(x=>x.type==='input_image'),[img(1),img(2),img(3)]);
 assert.match(a[3].content[4].text,/tool call b/);
});
test('text-only and empty tool outputs normalize without attachments; unsupported media is not discarded',()=>{
 const input=[{type:'function_call_output',call_id:'a',output:[{type:'input_text',text:'one'},{type:'text',text:'two'}]},{type:'function_call_output',call_id:'b',output:[]}];
 assert.deepEqual(adaptLocalResponsesTools({input},{imageToolResultsAsMessages:true}).request.input.map(i=>i.output),['one\ntwo','']);
 for(const block of [{type:'input_audio'},{type:'input_image'},{}])assert.throws(()=>adaptLocalResponsesTools({input:[{type:'function_call_output',call_id:'a',output:[block]}]},{imageToolResultsAsMessages:true}),e=>e.status===400&&/Unsupported local tool output block/.test(e.message));
});
test('namespace becomes a stable function alias and Codex call identity is restored',()=>{
 const source={tools:[tool],input:[{type:'message',role:'user',content:'hi'}]};const before=JSON.stringify(source);
 const a=adaptLocalResponsesTools(source),f=a.request.tools[0];assert.equal(f.type,'function');assert.match(f.name,/^office_ns_/);assert.deepEqual(f.parameters,tool.tools[0].parameters);assert.match(f.description,/Read state first/);assert.equal(JSON.stringify(source),before);
 const response={output:[{type:'function_call',id:'fc1',call_id:'c1',name:f.name,arguments:'{"code":"await cua.getState()"}'}]};
 const restored=a.restoreResponse(response);assert.equal(restored.output[0].name,'js');assert.equal(restored.output[0].namespace,tool.name);assert.equal(restored.output[0].call_id,'c1');
 const resume=adaptLocalResponsesTools({tools:[tool],input:[restored.output[0],{type:'function_call_output',call_id:'c1',output:'result'}]});assert.equal(resume.request.input[0].name,f.name);assert.equal(resume.request.input[0].namespace,undefined);assert.equal(resume.request.input[1].output,'result');
});
test('namespace SSE restores added/done and completion output, preserves usage and call arguments',async()=>{
 const a=adaptLocalResponsesTools({tools:[tool]}),alias=a.request.tools[0].name;
 const item={type:'function_call',name:alias,call_id:'c1',arguments:'{"code":"한글"}'};
 const events=[{type:'response.output_item.added',item},{type:'response.output_item.done',item},{type:'response.completed',response:{output:[item],usage:{input_tokens:10,output_tokens:2}}}];
 const text=events.map(e=>'data: '+JSON.stringify(e)+'\r\n\r\n').join('');let output='';for await(const c of Readable.from([...Buffer.from(text)].map(b=>Buffer.from([b]))).pipe(a.stream()))output+=c;
 const mapped=output.trim().split('\r\n\r\n').map(f=>JSON.parse(f.slice(6)));assert.equal(mapped[0].item.namespace,tool.name);assert.equal(mapped[1].item.arguments,item.arguments);assert.deepEqual(mapped[2].response.usage,{input_tokens:10,output_tokens:2});
});
test('ordinary tool names and unknown calls are never reinterpreted; collisions rejected',()=>{
 const a=adaptLocalResponsesTools({tools:[tool]});assert.throws(()=>adaptLocalResponsesTools({tools:[tool,{type:'function',name:a.request.tools[0].name}]}),/collision/);
 const item={type:'function_call',name:'not-advertised',arguments:'{}'};assert.equal(a.restoreResponse({output:[item]}).output[0],item);
});
test('client tool search round trips and discovered namespaces become callable on the next request',()=>{
 const search={type:'tool_search',execution:'client',description:'Search available connectors',parameters:{type:'object',properties:{query:{type:'string'}},required:['query']}};
 const a=adaptLocalResponsesTools({tools:[search]});const f=a.request.tools[0];assert.equal(f.type,'function');assert.deepEqual(f.parameters,search.parameters);
 const call=a.restoreResponse({output:[{type:'function_call',name:f.name,call_id:'search1',arguments:'{"query":"Google Sheets"}',status:'completed'}]}).output[0];
 assert.equal(call.type,'tool_search_call');assert.equal(call.execution,'client');assert.deepEqual(call.arguments,{query:'Google Sheets'});
 const next=adaptLocalResponsesTools({tools:[search,tool],input:[call,{type:'tool_search_output',execution:'client',call_id:'search1',tools:[tool]}]});
 assert.equal(next.request.input[0].type,'function_call');assert.equal(next.request.input[1].type,'function_call_output');assert.equal(next.request.input[1].call_id,'search1');assert.equal(next.request.tools.length,2);
 assert.equal(next.restoreResponse({output:[{type:'function_call',name:next.request.tools.find(t=>t.name.startsWith('office_ns_')).name,call_id:'c2',arguments:'{}'}]}).output[0].namespace,tool.name);
 assert.throws(()=>a.restoreResponse({output:[{type:'function_call',name:f.name,arguments:'broken'}]}),SyntaxError);
});
test('freeform apply_patch is exposed as a string parameter and restored without executing it',()=>{
 const custom={type:'custom',name:'apply_patch',description:'Patch files'};const a=adaptLocalResponsesTools({tools:[custom]});
 const patch='*** Begin Patch\n*** Add File: test.txt\n+한글\n*** End Patch';
 const call=a.restoreResponse({output:[{type:'function_call',name:a.request.tools[0].name,call_id:'patch1',arguments:JSON.stringify({input:patch})}]}).output[0];
 assert.equal(call.type,'custom_tool_call');assert.equal(call.input,patch);assert.equal(call.name,'apply_patch');
 const b=adaptLocalResponsesTools({tools:[custom],input:[call,{type:'custom_tool_call_output',call_id:'patch1',output:'ok'}]});
 assert.equal(b.request.input[0].name,a.request.tools[0].name);assert.deepEqual(JSON.parse(b.request.input[0].arguments),{input:patch});assert.equal(b.request.input[1].type,'function_call_output');
});
test('historical namespace and custom aliases cannot shadow current ordinary functions',()=>{
 const custom={type:'custom',name:'apply_patch'};
 for(const source of [tool,custom]){
  const a=adaptLocalResponsesTools({tools:[source]});const name=a.request.tools[0].name;
  const previous=a.restoreResponse({output:[{type:'function_call',name,call_id:'old',arguments:source===custom?'{"input":"patch"}':'{}'}]}).output[0];
  assert.throws(()=>adaptLocalResponsesTools({tools:[{type:'function',name,parameters:{type:'object'}}],input:[previous]}),/collision/);
  assert.throws(()=>adaptLocalResponsesTools({tools:[source],input:[{type:'function_call',name,call_id:'ordinary',arguments:'{}'}]}),/collision/);
 }
});
test('current schemas override historical schemas, including merged namespace children',()=>{
 const old={...tool,tools:[{...tool.tools[0],description:'old',parameters:{type:'object',properties:{old:{type:'string'}}}},{type:'function',name:'second',parameters:{type:'object'}}]};
 const a=adaptLocalResponsesTools({tools:[tool],input:[{type:'tool_search_output',execution:'client',call_id:'s1',tools:[old]}]});
 assert.equal(a.request.tools.length,2);
 const current=a.request.tools.find(t=>t.description?.includes('Read state first'));assert.deepEqual(current.parameters,tool.tools[0].parameters);
 assert.ok(a.request.tools.some(t=>t.description?.includes('.second')));
 const plain={type:'function',name:'read',parameters:{type:'object'},description:'current'};
 const b=adaptLocalResponsesTools({tools:[plain],input:[{type:'tool_search_output',execution:'client',call_id:'s1',tools:[{...plain,description:'old'}]}]});assert.equal(b.request.tools[0].description,'current');
});
