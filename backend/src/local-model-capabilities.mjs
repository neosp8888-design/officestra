// Capabilities are pinned to validated artifacts/runtime, not inferred from names.
export const MEROMERO_MODEL_ID='officestra-meromero-31b-uncensored-heretic-q4km';
export const MEROMERO_HOST_KEY='g4-meromero-31b-uncensored-heretic';
export const MEROMERO_ARTIFACT={
 root:'G:\\llm\\llmfan46\\G4-MeroMero-31B-uncensored-heretic-GGUF',
 file:'G4-MeroMero-31B-uncensored-heretic-Q4_K_M.gguf',
 size:18687063776,
 sha256:'A33DAFFCE3B76ADA423616D7CCBE7A70724DA854F8175605967B290CE3D9EF0C',
};
export const isMeroMero=profile=>profile?.runtime==='llama-cpp-b10982'&&profile.model===MEROMERO_MODEL_ID;
export function directReasoningOptions(profile){
 return isMeroMero(profile)?['default','off','on']:['default','low','medium','xhigh'];
}
export function directDefaultReasoning(profile){return isMeroMero(profile)?'off':'xhigh';}
export function directCodexReasoning(profile){
 const selected=profile.reasoning&&profile.reasoning!=='default'?profile.reasoning:directDefaultReasoning(profile);
 return isMeroMero(profile)?selected==='on'?'medium':'none':selected;
}
