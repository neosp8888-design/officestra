// Opt-in local launch specification. No server, tunnel, worker, or DB mutation.
// The caller must provision an authenticated loopback compatibility bridge and
// persist the provider snapshot per turn before enabling this for an employee.
import { createHash } from 'node:crypto';
import { accessSync, constants, statSync } from 'node:fs';
import { isAbsolute, join } from 'node:path';
import { buildArguments, claudePersistentArguments, executionEnvironment, locateExecutable,
  claudePersistentWorkerSignature, normalizeAutoCompactPercent } from './agent-runtime.mjs';
import { terminalArguments } from './terminal-sessions.mjs';

const KEYS = new Set(['id','providerKind','backend','model','endpoint','credentialEnv',
  'credentialVersion','contextWindow','maxOutputTokens','usageProtocol','reasoning','kvCacheQuantization','runtime']);
const text = (value, name) => {
  if (typeof value !== 'string' || !value.trim() || /[\r\n\0]/.test(value)) {
    throw new TypeError(`Invalid local profile ${name}`);
  }
  return value;
};

export function normalizeLocalAgentProfile(value) {
  if (!value || typeof value !== 'object' || Array.isArray(value) ||
      Object.keys(value).some(key => !KEYS.has(key))) throw new TypeError('Invalid local profile fields');
  if (value.providerKind !== 'local' || !['claude','codex'].includes(value.backend)) {
    throw new TypeError('Only explicit local Claude or Codex runner profiles are supported');
  }
  const id=text(value.id,'id');
  if (!/^[a-z0-9][a-z0-9-]{0,63}$/.test(id)) throw new TypeError('Invalid local profile ID');
  const model=text(value.model,'model');
  const url=new URL(text(value.endpoint,'endpoint'));
  if (url.protocol!=='http:' || !['127.0.0.1','[::1]'].includes(url.hostname) ||
      !url.port || url.username || url.password || url.search || url.hash || url.pathname!=='/') {
    throw new TypeError('Local endpoint must be a credential-free loopback bridge URL');
  }
  const credentialEnv=text(value.credentialEnv,'credentialEnv');
  if (!/^OFFICESTRA_LOCAL_[A-Z0-9_]+_TOKEN$/.test(credentialEnv)) throw new TypeError('Use a dedicated local credential environment reference');
  const credentialVersion=text(value.credentialVersion,'credentialVersion');
  if (![32768,65536].includes(value.contextWindow) || !Number.isSafeInteger(value.maxOutputTokens) || value.maxOutputTokens<1 || value.maxOutputTokens>value.contextWindow) throw new TypeError('Local output budget must be a safe integer within the context window');
  const expectedUsageProtocol=value.backend==='codex'
    ? 'openai-responses-v1'
    : 'anthropic-normalized-v1';
  if (value.usageProtocol!==expectedUsageProtocol) {
    throw new TypeError(`Local ${value.backend} profile requires ${expectedUsageProtocol}`);
  }
  if(value.runtime!==undefined&&value.runtime!=='llama-cpp-b10982')throw new TypeError('Unsupported local runtime');
  if(value.runtime&&!(value.backend==='codex'&&value.contextWindow===65536&&value.kvCacheQuantization==='q8_0'&&model==='officestra-qwen38-27b-uncensored-q4km'))throw new TypeError('Direct runtime requires verified Qwen 64K KV8');
  if(value.reasoning!==undefined&&!(value.runtime?['default','low','medium','xhigh']:['default','on','off']).includes(value.reasoning))throw new TypeError('Unsupported local reasoning option');
  if(value.kvCacheQuantization!==undefined &&
      !(value.kvCacheQuantization==='q8_0'&&value.backend==='codex'&&value.contextWindow===65536)) {
    throw new TypeError('Only explicit local Codex 64K q8 KV is supported');
  }
  return Object.freeze({id,providerKind:'local',backend:value.backend,model,
    endpoint:url.origin,credentialEnv,credentialVersion,contextWindow:value.contextWindow,
    maxOutputTokens:value.maxOutputTokens,usageProtocol:value.usageProtocol,
    ...(value.reasoning!==undefined?{reasoning:value.reasoning}:{}),
    ...(value.runtime?{runtime:value.runtime}:{}),
    ...(value.kvCacheQuantization!==undefined?{kvCacheQuantization:value.kvCacheQuantization}:{})});
}

function localClaudeOptions(args, mode) {
  const result=[];
  for (let i=0;i<args.length;i++) {
    // User prompt and identity are data, even if they look like an option.
    if ((args[i]==='-p' && mode==='gui') ||
        ['--settings','--permission-mode','--model','--append-system-prompt','--resume',
          '--output-format','--input-format'].includes(args[i])) {
      result.push(args[i],args[++i]);continue;
    }
    // These flags belong to the cloud model. Gateway effort delivery has not
    // been verified, so do not offer a setting that is silently ignored.
    if (args[i]==='--effort') {i++;continue;}
    if (args[i]==='--prompt-suggestions') {result.push(args[i],'false');i++;continue;}
    result.push(args[i]);
  }
  return [...result,'--setting-sources',''];
}

const LOCAL_CODEX_PROVIDER='officestra_local';
export function localTerminalExecutable(executable,env){
  if(isAbsolute(executable))return executable;
  // SwiftTerm calls execve, unlike Node spawn it does not search PATH.
  // Ignore relative PATH entries so an employee workdir cannot shadow the CLI.
  for(const directory of String(env.PATH??'').split(':')){
    if(!isAbsolute(directory))continue;
    const candidate=join(directory,executable);
    try{accessSync(candidate,constants.X_OK);if(statSync(candidate).isFile())return candidate;}catch{}
  }
  throw new Error(`Local terminal executable not found: ${executable}`);
}
const CODEX_CLOUD_CONFIG_KEYS=new Set([
  'model','model_reasoning_effort','features.fast_mode','service_tier',
  'model_reasoning_summary','show_raw_agent_reasoning','hide_agent_reasoning',
]);

function quotedConfig(value) {
  return JSON.stringify(String(value));
}

function localCodexOptions(args, profile, character, mode, catalogPath, workdir) {
  const result=[];
  for(let i=0;i<args.length;i++) {
    if(args[i]==='-c' && i+1<args.length) {
      const config=args[i+1];
      const key=String(config).split('=',1)[0];
      if(CODEX_CLOUD_CONFIG_KEYS.has(key)) {i++;continue;}
      result.push(args[i],config);i++;continue;
    }
    result.push(args[i]);
  }
  const autoCompactPercent=normalizeAutoCompactPercent(character.autoCompactPercent);
  const autoCompactTokenLimit=Math.floor(profile.contextWindow*autoCompactPercent/100);
  const overrides=[
    '-c',`model=${quotedConfig(profile.model)}`,
    '-c',`model_provider=${quotedConfig(LOCAL_CODEX_PROVIDER)}`,
    '-c',`model_reasoning_effort=${quotedConfig(profile.runtime==='llama-cpp-b10982'?(profile.reasoning&&profile.reasoning!=='default'?profile.reasoning:'xhigh'):'default')}`,
    // Display the reasoning this local model actually emits. This does not
    // change effort or synthesize a summary; both GUI JSON and TUI need it.
    // llama.cpp raw content is mirrored into the Codex summary display lane
    // by our bridge. Do not print the same content twice in the native TUI.
    '-c',`show_raw_agent_reasoning=${profile.runtime==='llama-cpp-b10982'?'false':'true'}`,
    '-c','hide_agent_reasoning=false',
    '-c','model_reasoning_summary="detailed"',
    '-c',`features.fast_mode=false`,
    '-c',`service_tier=${quotedConfig('default')}`,
    '-c',`model_providers.${LOCAL_CODEX_PROVIDER}.name=${quotedConfig('OFFICESTRA Local')}`,
    '-c',`model_providers.${LOCAL_CODEX_PROVIDER}.base_url=${quotedConfig(`${profile.endpoint}/v1`)}`,
    '-c',`model_providers.${LOCAL_CODEX_PROVIDER}.wire_api=${quotedConfig('responses')}`,
    '-c',`model_providers.${LOCAL_CODEX_PROVIDER}.env_key=${quotedConfig(profile.credentialEnv)}`,
    '-c',`model_providers.${LOCAL_CODEX_PROVIDER}.requires_openai_auth=false`,
    '-c',`model_providers.${LOCAL_CODEX_PROVIDER}.supports_websockets=false`,
    // An aborted/failed local request must not silently reload a 24 GB GPU
    // workload. Reconnect only when the user sends a new request.
    '-c',`model_providers.${LOCAL_CODEX_PROVIDER}.request_max_retries=0`,
    '-c',`model_providers.${LOCAL_CODEX_PROVIDER}.stream_max_retries=0`,
    '-c',`model_context_window=${profile.contextWindow}`,
    '-c',`model_auto_compact_token_limit=${autoCompactTokenLimit}`,
    '-c',`model_auto_compact_token_limit_scope=${quotedConfig('total')}`,
  ];
  if(catalogPath)overrides.push('-c',`model_catalog_json=${quotedConfig(catalogPath)}`);
  if(mode!=='gui')return [...result,...overrides,'-C',workdir];
  const prompt=result.pop();
  return [...result,...overrides,prompt];
}

export function createLocalAgentLaunch({profile,character,mode='gui',prompt='',
  previousSessionID=null,workdir,baseEnvironment=process.env,hookPath,nodePath,catalogPath,
  executable=locateExecutable(character)}) {
  const p=normalizeLocalAgentProfile(profile);
  if (!character || character.backend!==p.backend || character.model!==p.model) throw new TypeError('Character runner/model must match the local profile');
  if (character.fastMode===true) throw new TypeError('Local Fast mode is not verified');
  if (character.effort!=='default') throw new TypeError('Local profile requires default effort; configurable effort is not verified');
  const allowedPermissions=p.backend==='codex'
    ? ['read-only','workspace-write','danger-full-access']
    : ['plan','auto','acceptEdits','bypassPermissions'];
  if (!allowedPermissions.includes(character.permission)) throw new TypeError(`Invalid ${p.backend} permission`);
  if (typeof workdir!=='string' || !workdir.startsWith('/') || /[\r\n\0]/.test(workdir)) throw new TypeError('Absolute workdir required');
  if (!['gui','terminal','persistent'].includes(mode)) throw new TypeError('Invalid local execution mode');
  if (p.backend==='codex' && mode==='persistent') throw new TypeError('Persistent local Codex workers are not supported');
  if (previousSessionID!==null && (typeof previousSessionID!=='string' || !previousSessionID.trim() || /[\r\n\0]/.test(previousSessionID))) throw new TypeError('Invalid resume session');
  const credential=baseEnvironment[p.credentialEnv];
  if (typeof credential!=='string' || !credential || /[\r\n\0]/.test(credential)) throw new TypeError('Local bridge credential is unavailable');
  const clean=Object.fromEntries(Object.entries(baseEnvironment).filter(([key]) =>
    !/^(?:ANTHROPIC_|CLAUDE_|OPENAI_|OFFICESTRA_LOCAL_|DISABLE_AUTO_COMPACT$|DISABLE_COMPACT$|MAX_THINKING_TOKENS$)/.test(key)));
  const env=executionEnvironment(character,clean,{workdir});
  delete env.CLAUDE_AUTOCOMPACT_PCT_OVERRIDE;
  if(p.backend==='claude') {
    Object.assign(env,{
      ANTHROPIC_BASE_URL:p.endpoint,ANTHROPIC_AUTH_TOKEN:credential,ANTHROPIC_API_KEY:credential,
      CLAUDE_CODE_USE_GATEWAY:'1',CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC:'1',
      CLAUDE_CODE_MAX_RETRIES:'0',CLAUDE_CODE_MAX_CONTEXT_TOKENS:String(p.contextWindow),
      CLAUDE_CODE_MAX_OUTPUT_TOKENS:String(p.maxOutputTokens),MAX_THINKING_TOKENS:'0',
      // A local model can spend several minutes loading and evaluating a long
      // prompt before it sends response headers. Claude Code otherwise applies
      // both its SDK request timeout and independent first-byte/stream
      // watchdogs, which can terminate a healthy generation. OFFICESTRA owns
      // cancellation through the child process and local bridge instead.
      // Claude's SDK treats zero as an immediate timeout. Use the largest
      // delay Node can schedule without overflowing to a near-zero timer
      // (about 24.8 days), which is effectively unbounded for one turn.
      API_TIMEOUT_MS:'2147483647',CLAUDE_ENABLE_BYTE_WATCHDOG:'0',
      CLAUDE_ENABLE_STREAM_WATCHDOG:'0',
      ANTHROPIC_DEFAULT_HAIKU_MODEL:p.model,ANTHROPIC_DEFAULT_SONNET_MODEL:p.model,
      ANTHROPIC_DEFAULT_OPUS_MODEL:p.model,DISABLE_TELEMETRY:'1',
    });
  } else {
    env[p.credentialEnv]=credential;
  }
  const args=mode==='terminal'
    ? terminalArguments({character,previousSessionID,workdir,hookPath,nodePath})
    : mode==='persistent' ? claudePersistentArguments(character,previousSessionID)
    : buildArguments({character,prompt,previousSessionID,workdir});
  const normalizedArgs=p.backend==='codex'
    ? localCodexOptions(args,p,character,mode,catalogPath,workdir)
    : localClaudeOptions(args,mode);
  // Secret-free worker identity; callers must rotate credentialVersion with a
  // credential change. Do not put credential values into logs/signatures.
  const workerIdentity=claudePersistentWorkerSignature({character,workdir,executable});
  const signature=createHash('sha256').update(JSON.stringify({profile:p,mode,workerIdentity})).digest('hex');
  return {executable:mode==='terminal'?localTerminalExecutable(executable,env):executable,backend:p.backend,providerKind:'local',profile:p,args:normalizedArgs,env,cwd:workdir,signature};
}
