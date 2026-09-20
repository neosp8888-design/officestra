// Capabilities are pinned to validated artifacts/runtime, not inferred from names.
export const MEROMERO_MODEL_ID='officestra-meromero-31b-uncensored-heretic-q4km';
export const MEROMERO_HOST_KEY='g4-meromero-31b-uncensored-heretic';
export const MEROMERO_ARTIFACT={
 root:'G:\\llm\\llmfan46\\G4-MeroMero-31B-uncensored-heretic-GGUF',
 file:'G4-MeroMero-31B-uncensored-heretic-Q4_K_M.gguf',
 size:18687063776,
 sha256:'A33DAFFCE3B76ADA423616D7CCBE7A70724DA854F8175605967B290CE3D9EF0C',
};
export const MEROMERO_VISION_ARTIFACT={
 root:MEROMERO_ARTIFACT.root,
 file:'G4-MeroMero-31B-uncensored-heretic-mmproj-BF16.gguf',
 size:1200727200,
 sha256:'7555F7CB379C138A01C7A70298747AD4451C9641FBD42E09EE7C86D61150EA49',
};
export const MEROMERO_26B_MODEL_ID='officestra-meromero-26b-a4b-uncensored-heretic-q4km';
export const MEROMERO_26B_HOST_KEY='g4-meromero-26b-a4b-it-uncensored-heretic';
export const MEROMERO_26B_ARTIFACT={
 root:'G:\\llm\\llmfan46\\G4-MeroMero-26B-A4B-it-uncensored-heretic-GGUF',
 file:'G4-MeroMero-26B-A4B-it-uncensored-heretic-Q4_K_M.gguf',
 size:17331776896,
 sha256:'9101D86B7F2AECD90FF0FB5F43AE73B2CBE580D335011FD60399AA864A3FE7E1',
};
export const MEROMERO_26B_VISION_ARTIFACT={
 root:MEROMERO_26B_ARTIFACT.root,
 file:'G4-MeroMero-26B-A4B-it-uncensored-heretic-mmproj-BF16.gguf',
 size:1194827840,
 sha256:'A4DD63C9C0B67098CE25CF85359B6E4B325BB719F65590AD8E8ACD5ED77731F7',
};
export const isMeroMeroModel=model=>[MEROMERO_MODEL_ID,MEROMERO_26B_MODEL_ID].includes(model);
export const isMeroMero=profile=>profile?.runtime==='llama-cpp-b10982'&&isMeroMeroModel(profile.model);
export const localModelSupportsVision=profile=>profile?.model!==MEROMERO_MODEL_ID;
export function meroRuntimeArtifacts(profile){
 if(!isMeroMero(profile))throw new TypeError('Unsupported MeroMero profile');
 return profile.model===MEROMERO_26B_MODEL_ID
  ? {model:MEROMERO_26B_ARTIFACT,vision:MEROMERO_26B_VISION_ARTIFACT,hostKey:MEROMERO_26B_HOST_KEY}
  : {model:MEROMERO_ARTIFACT,vision:null,hostKey:MEROMERO_HOST_KEY};
}
export function directReasoningOptions(profile){
 return isMeroMero(profile)?['default','off','on']:['default','low','medium','xhigh'];
}
export function directDefaultReasoning(profile){return isMeroMero(profile)?'off':'xhigh';}
export function directCodexReasoning(profile){
 const selected=profile.reasoning&&profile.reasoning!=='default'?profile.reasoning:directDefaultReasoning(profile);
 return isMeroMero(profile)?selected==='on'?'medium':'none':selected;
}
