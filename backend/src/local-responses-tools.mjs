import { createHash } from 'node:crypto';
import { Transform } from 'node:stream';
import { StringDecoder } from 'node:string_decoder';

// LM Studio's Qwen template exposes function tools, but not Responses namespace
// wrappers. Flatten only the wire representation, then restore Codex's original
// names. The bridge never executes tools or changes their permission checks.
export function adaptLocalResponsesTools(value) {
  const aliases=new Map(), customAliases=new Map();
  const searchName='officestra_client_tool_search';
  const aliasFor=(namespace,name)=>'office_ns_'+createHash('sha256').update(JSON.stringify([namespace,name])).digest('hex').slice(0,32);
  const register=(namespace,name)=>{
    const alias=aliasFor(namespace,name);aliases.set(alias,{namespace,name});return alias;
  };
  const customFunction=(tool,namespace)=>{
    const name='office_custom_'+createHash('sha256').update(JSON.stringify([namespace??null,tool.name])).digest('hex').slice(0,32);
    customAliases.set(name,{name:tool.name,...(namespace?{namespace}:{})});
    return {type:'function',name,description:`${namespace?namespace+'.':''}${tool.name}\n${tool.description??''}\nPass the exact free-form tool input in input.`,parameters:{type:'object',properties:{input:{type:'string'}},required:['input'],additionalProperties:false}};
  };
  const declared=[];
  // Codex returns discovered connector schemas in history, not necessarily in
  // tools. LM Studio needs those schemas explicitly in its function list.
  for(const item of Array.isArray(value.input)?value.input:[]){
    if(item?.type==='tool_search_output'&&item.execution==='client')declared.push(...(item.tools??[]));
  }
  // Newer search results override older ones, and the current request's schema
  // is authoritative over all historical schemas (including namespace children).
  declared.push(...(value.tools??[]));
  const hasSearch=declared.some(t=>t?.type==='tool_search'&&t.execution==='client');
  const tools=declared.flatMap(tool=>{
    if(tool?.type==='tool_search'&&tool.execution==='client')return [{type:'function',name:searchName,description:tool.description,parameters:tool.parameters}];
    if(tool?.type==='custom')return [customFunction(tool)];
    if(tool?.type!=='namespace')return [tool];
    if(typeof tool.name!=='string'||!Array.isArray(tool.tools))throw new Error('Malformed tool namespace');
    return tool.tools.map(child=>{
      if(child.type==='custom')return customFunction(child,tool.name);
      if(child.type!=='function'||typeof child.name!=='string')throw new Error('Unsupported namespaced local tool');
      return {...child,name:register(tool.name,child.name),description:`${tool.name}.${child.name}\n${tool.description??''}\n${child.description??''}`};
    });
  });
  const input=Array.isArray(value.input)?value.input.map(item=>{
    if(item?.type==='custom_tool_call')return {type:'function_call',call_id:item.call_id,name:customFunction(item,item.namespace).name,arguments:JSON.stringify({input:item.input})};
    if(item?.type==='custom_tool_call_output')return {...item,type:'function_call_output'};
    if(item?.type==='tool_search_call'&&item.execution==='client')return {type:'function_call',call_id:item.call_id,name:searchName,arguments:JSON.stringify(item.arguments??{})};
    if(item?.type==='tool_search_output'&&item.execution==='client')return {type:'function_call_output',call_id:item.call_id,output:JSON.stringify({loaded_tools:(item.tools??[]).flatMap(t=>t.type==='namespace'?(t.tools??[]).map(child=>`${t.name}.${child.name}`):[t.name]),note:'The current callable schemas are included in the tools list.'})};
    if(item?.type!=='function_call'||!item.namespace)return item;
    const alias=register(item.namespace,item.name);
    const {namespace,...rest}=item;return {...rest,name:alias};
  }):value.input;
  // Replay registers aliases too. Validate only after replay, otherwise a
  // current ordinary function can be mistaken for a historical custom/MCP call.
  const historical=Array.isArray(value.input)?value.input:[];
  const searchUsed=hasSearch||historical.some(i=>i?.type==='tool_search_call'&&i.execution==='client');
  for(const tool of declared)if(tool?.type==='function'&&(aliases.has(tool.name)||customAliases.has(tool.name)||(searchUsed&&tool.name===searchName)))throw new Error('Local tool alias collision');
  for(const item of historical)if(item?.type==='function_call'&&!item.namespace&&(aliases.has(item.name)||customAliases.has(item.name)||(searchUsed&&item.name===searchName)))throw new Error('Historical local tool alias collision');
  const restoreItem=item=>{
    if(item?.type==='function_call'&&customAliases.has(item.name)){
      const args=item.arguments?JSON.parse(item.arguments):{input:''};
      if(typeof args.input!=='string')throw new Error('Invalid custom tool input');
      return {type:'custom_tool_call',...(item.id?{id:item.id}:{}),call_id:item.call_id,...customAliases.get(item.name),input:args.input,...(item.status?{status:item.status}:{})};
    }
    if(item?.type==='function_call'&&hasSearch&&item.name===searchName){
      return {type:'tool_search_call',...(item.id?{id:item.id}:{}),execution:'client',call_id:item.call_id,status:item.status??'completed',arguments:item.arguments?JSON.parse(item.arguments):{}};
    }
    if(item?.type!=='function_call'||!aliases.has(item.name))return item;
    return {...item,...aliases.get(item.name)};
  };
  const restoreResponse=response=>Array.isArray(response?.output)?{...response,output:response.output.map(restoreItem)}:response;
  const restoreEvent=event=>{
    if(event?.item)return {...event,item:restoreItem(event.item)};
    if(event?.response)return {...event,response:restoreResponse(event.response)};
    return event;
  };
  const uniqueTools=[...new Map(tools.map(t=>[t.name??t.type,t])).values()];
  return {request:{...value,...(declared.length?{tools:uniqueTools}:{}),...(value.input!==undefined?{input}:{})},restoreResponse,
    stream:()=>jsonSSETransform(restoreEvent)};
}

function jsonSSETransform(map) {
  const decoder=new StringDecoder('utf8');let pending='';
  const frame=raw=>{
    const lines=raw.split(/\r?\n/),data=lines.filter(l=>l.startsWith('data:'));
    if(!data.length)return raw;
    let event;try{event=JSON.parse(data.map(l=>l.slice(5).trimStart()).join('\n'));}catch{return raw;}
    const mapped=map(event);if(mapped===event)return raw;
    let emitted=false;return lines.flatMap(line=>{
      if(!line.startsWith('data:'))return [line];if(emitted)return [];emitted=true;return ['data: '+JSON.stringify(mapped)];
    }).join(raw.includes('\r\n')?'\r\n':'\n');
  };
  function drain(stream,final=false){
    let match;while((match=/\r?\n\r?\n/.exec(pending))){
      if(match.index>8*1024*1024)throw new Error('Oversized tool SSE frame');
      stream.push(frame(pending.slice(0,match.index))+match[0]);pending=pending.slice(match.index+match[0].length);
    }
    if(pending.length>8*1024*1024)throw new Error('Oversized tool SSE frame');
    if(final&&pending){stream.push(frame(pending));pending='';}
  }
  return new Transform({transform(chunk,_encoding,callback){try{pending+=decoder.write(chunk);drain(this);callback();}catch(e){callback(e);}},flush(callback){try{pending+=decoder.end();drain(this,true);callback();}catch(e){callback(e);}}});
}
