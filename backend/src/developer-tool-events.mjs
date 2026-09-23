// Optional developer hooks can report progress through the normal activity UI.
// No hook installation, external executable or Graft dependency is required.
export function developerToolEnvironment(characterID, base = process.env, mode = 'gui') {
  const port = Number(base.OFFICE_BACKEND_PORT ?? 4317);
  return {
    OFFICESTRA_TOOL_CHARACTER_ID: String(characterID),
    OFFICESTRA_TOOL_MODE: mode,
    OFFICESTRA_TOOL_EVENTS_URL: `http://127.0.0.1:${port}/api/developer-tool-events`,
  };
}

export function parseDeveloperToolEvent(body) {
  const { characterId, sessionId, mode, activity } = body ?? {};
  if (typeof characterId !== 'string' || !characterId || characterId.length > 100 ||
      typeof sessionId !== 'string' || !sessionId || sessionId.length > 200 ||
      !['gui', 'terminal'].includes(mode) ||
      !/^developer-tool:[a-z0-9:-]{1,130}$/.test(activity?.eventKey ?? '') ||
      !['running', 'completed'].includes(activity?.status) ||
      typeof activity?.text !== 'string' || !activity.text.trim() || activity.text.length > 300) {
    return null;
  }
  return { characterId, sessionId, mode, activity: {
    kind: 'tool', eventKey: activity.eventKey, status: activity.status,
    text: activity.text.replace(/[\x00-\x1f\x7f]/g, ' ').trim(),
  } };
}

export async function recordDeveloperToolEvent(runtime, body) {
  const event = parseDeveloperToolEvent(body);
  if (!event) return { accepted: false, reason: 'invalid_event' };
  let state;
  if (event.mode === 'gui') {
    state = runtime.running.get(event.characterId);
    if (!state || state.cancelRequested || state.externalSessionID !== event.sessionId) {
      return { accepted: false, reason: 'inactive_session' };
    }
  } else {
    const terminal = runtime.terminalSessionRegistry?.sessions.get(event.characterId);
    if (!terminal || terminal.closed || !terminal.runningTurnID ||
        terminal.externalSessionID !== event.sessionId || terminal.toolEventsClosing) {
      return { accepted: false, reason: 'inactive_session' };
    }
    if (terminal.toolActivityState?.turnID !== terminal.runningTurnID) {
      terminal.toolActivityState = {
        turnID: terminal.runningTurnID, character: { id: event.characterId },
        sequence: 0, activityRecords: new Map(), activityWritePromise: null,
      };
    }
    state = terminal.toolActivityState;
  }
  state.developerToolEventKeys ??= new Set();
  if (!state.developerToolEventKeys.has(event.activity.eventKey) && state.developerToolEventKeys.size >= 40) {
    return { accepted: false, reason: 'event_limit' };
  }
  state.developerToolEventKeys.add(event.activity.eventKey);
  await runtime.addActivity(state, event.activity);
  return { accepted: true };
}
