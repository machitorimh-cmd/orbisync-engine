// Browser-only startup/probe presentation. Only known codes reach the DOM;
// credentials and raw response/error bodies never enter diagnostic rendering.
const PROBE_STEPS = ['http', 'websocket', 'join', 'sync'];
const STEP_IDS = { http: 'Http', websocket: 'Websocket', join: 'Join', sync: 'Sync' };
const STATE_TEXT = { passed: 'Passed', failed: 'Failed', skipped: 'Not reached', running: 'Checking…', waiting: 'Not tested', stopped: 'Stopped', not_started: 'Not started' };
const PROBLEMS = {
  runtime_missing: 'The bundled database runtime is missing. Extract the complete server distribution, then check again, or use your own database.',
  version_mismatch: 'Use the database runtime supplied with this installation. Existing database files were preserved.',
  private_storage: 'The private installation folder cannot be used. Check its permissions, then try again.',
  in_use: 'Another launcher is using this installation. Close that launcher, then try again.',
  recovery_required: 'Database files need operator recovery. They were preserved; do not initialize over them.',
  identity_mismatch: 'The database identity could not be verified. No unrelated process was stopped. Check the installation before retrying.',
  database_identity_mismatch: 'The database identity could not be verified. No unrelated process was stopped. Check the installation before retrying.',
  port_in_use: 'The database port is in use. Choose another database port; other applications will not be stopped.',
  engine_port_in_use: 'The engine port is in use. Choose an available port; other applications will not be stopped.',
  initialization_failed: 'The database could not be initialized. Check the bundled runtime and folder permissions, then retry.',
  start_failed: 'The database could not start. Check the bundled runtime and folder permissions, then retry.',
  readiness_timeout: 'The database did not become ready. Check local resources, then retry.',
  provisioning_failed: 'Local database preparation failed. Its files were kept. Check setup, then retry.',
  stop_failed: 'The database did not stop cleanly. Check setup before trying again.',
  database_unavailable: 'The database is unavailable. Retry local startup or check your database settings.',
  configuration_invalid: 'Check the saved settings, then retry startup.',
  corpus_missing: 'The password corpus is missing. Check the saved password policy before retrying.',
  corpus_invalid: 'The password corpus is invalid. Check the saved password policy before retrying.',
  signing_key_invalid: 'The saved signing key could not be loaded. Check the private installation before retrying.',
  initialization_required: 'Complete initialization before starting the engine.',
  migration_failed: 'The database needs attention. Check setup before retrying.',
  engine_start_failed: 'The engine could not start. Check setup, then retry startup.',
  engine_readiness_timeout: 'The engine did not become ready. Check setup, then retry startup.',
  engine_stop_timeout: 'The engine is still stopping. Check its status before trying again.',
  trial_credentials_changed: 'The local trial credentials changed. Sign in with the current administrator password.',
  trial_credentials_missing: 'The local trial credentials are unavailable. Sign in with your saved administrator password.',
  operation_in_progress: 'Another local operation is running. Wait for it to finish, then check status.',
  managed_database_fixed: 'Local trial uses its dedicated database. Use the database port repair action to change its port.',
  AUTHENTICATION_REQUIRED: 'Sign in before testing a connection.',
  UNAUTHORIZED: 'Sign in before testing a connection.',
  FORBIDDEN: 'Open the private launcher link and sign in with an administrator account.',
};

export function createLocalConnection({ request, refreshStatus, localize, onTrial, canProbe, showManual }) {
  const $ = id => document.getElementById(id);
  const text = (id, key) => localize($(id), key);
  let status = {}, busy = false, probeBusy = false, diagnosticsBusy = false;
  let operationEpoch = 0, probeEpoch = 0, diagnosticEpoch = 0, autoProbeDone = false;
  let localPhase = 'Not started', localFailed = false, probeResult = null;
  let problem = null, repairs = new Set();

  function problemText(code) { return typeof code === 'string' && Object.hasOwn(PROBLEMS, code) ? PROBLEMS[code] : 'The operation could not finish. Check setup, then retry.'; }
  function clearProbe() {
    probeEpoch++; probeBusy = false; probeResult = null; autoProbeDone = false;
    for (const step of PROBE_STEPS) setStep(step, 'waiting');
  }
  function setStep(step, value) {
    const name = STEP_IDS[step];
    $(`probeStep${name}`).dataset.state = value;
    text(`probe${name}State`, STATE_TEXT[value] || STATE_TEXT.waiting);
  }
  function showProblem(code, actions = []) {
    problem = problemText(code); repairs = new Set(actions);
    text('diagnosticSummary', problem); $('diagnosticSummary').hidden = false;
    $('retryDiagnostics').hidden = false;
    $('repairLocalStartup').hidden = !repairs.has('retry');
    $('chooseEnginePort').hidden = !repairs.has('choose_engine_port');
    $('chooseDatabasePort').hidden = !repairs.has('choose_database_port');
    $('diagnosticManualSetup').hidden = !!status.initialized;
  }
  function clearProblem() {
    problem = null; repairs.clear();
    for (const id of ['diagnosticSummary', 'retryDiagnostics', 'repairLocalStartup', 'chooseEnginePort', 'chooseDatabasePort', 'diagnosticManualSetup']) $(id).hidden = true;
  }
  function render() {
    const working = busy || probeBusy || status.operation?.busy === true;
    const managed = status.managed_db !== null && typeof status.managed_db === 'object';
    const databaseRunning = managed && ['running', 'ready', 'starting'].includes(status.managed_db.state);
    $('localTrial').hidden = !!status.existing || (!!status.initialized && !status.local_trial && !managed);
    $('startLocalTrial').hidden = !!status.initialized && !status.local_trial;
    text('startLocalTrial', status.initialized ? 'Continue local trial' : 'Try on this PC');
    $('startLocalTrial').disabled = working;
    $('retryLocalTrial').hidden = !localFailed;
    $('retryLocalTrial').disabled = working;
    $('showManualSetup').hidden = !!status.initialized;
    $('connectionTest').hidden = !status.initialized && !localFailed;
    $('startEngine').hidden = !status.initialized || status.engine === 'running';
    $('startEngine').disabled = working;
    $('engine').hidden = !status.initialized && !managed;
    $('stopEngine').hidden = status.engine !== 'running' && !status.ready && !databaseRunning;
    $('stopEngine').disabled = working;
    text('stopEngine', managed ? 'Stop engine and local database' : 'Stop engine');
    $('restartEngine').hidden = !status.initialized || status.engine !== 'running';
    $('restartEngine').disabled = working;
    for (const id of ['runProbe', 'retryProbe']) $(id).disabled = working || probeBusy || !status.local_trial || !status.ready || !canProbe();
    for (const id of ['runDiagnostics', 'retryDiagnostics']) $(id).disabled = diagnosticsBusy || working;
    for (const id of ['repairLocalStartup', 'chooseEnginePort', 'chooseDatabasePort']) $(id).disabled = working;
    document.querySelectorAll('#setupForm button, #loginForm button').forEach(button => { button.disabled = working; });
    $('retryProbe').hidden = !probeResult || probeResult.ok;
    const phase = status.operation?.busy ? status.operation.phase : '';
    const phases = {
      db_preparing: 'Preparing the local database…', db_starting: 'Starting the database…',
      initializing: 'Applying initial setup…', engine_starting: 'Starting the engine…', stopping: 'Stopping safely…',
      probing: 'Testing the connection…',
    };
    text('localPhase', phases[phase] || localPhase);
    const databaseReady = managed && ['running', 'ready'].includes(status.managed_db.state);
    const databaseWorking = working && (['db_preparing', 'db_starting'].includes(phase) || localPhase === 'Preparing the local database…');
    const states = {
      Database: databaseReady ? 'passed' : databaseWorking ? 'running' : managed && status.managed_db.state === 'stopped' ? 'stopped' : localFailed ? 'failed' : 'waiting',
      Setup: status.initialized ? 'passed' : phase === 'initializing' ? 'running' : 'waiting',
      Engine: status.ready ? 'passed' : working && (phase === 'engine_starting' || localPhase === 'Starting the engine…') ? 'running' : status.initialized && !working ? status.engine === 'not started' ? 'not_started' : 'stopped' : 'waiting',
      Connection: probeResult?.ok ? 'passed' : probeBusy ? 'running' : probeResult ? 'failed' : 'waiting',
    };
    for (const [name, value] of Object.entries(states)) {
      $(`localStage${name}`).dataset.state = value;
      text(`local${name}State`, STATE_TEXT[value] || STATE_TEXT.waiting);
    }
    if (!probeBusy && !probeResult) text('probeSummary', !status.local_trial ? 'The full connection test is available in local trial. Existing databases are not seeded.' : !status.ready ? 'Start the engine to test a connection.' : !canProbe() ? 'Sign in before testing a connection.' : 'Ready to test a real connection.');
  }
  function observe(next) {
    const wasReady = status.ready;
    status = next && typeof next === 'object' ? next : {};
    if (wasReady && !status.ready) clearProbe();
    if (!busy && !status.operation?.busy) localPhase = status.ready ? 'Ready' : status.initialized ? 'Stopped' : 'Not started';
    if (status.operation?.error_code && !busy) {
      localFailed = true; localPhase = 'Startup needs attention';
      showProblem(status.operation.error_code, ['retry']);
    }
    render();
    if (status.local_trial && status.ready && canProbe() && !busy && !probeBusy && !autoProbeDone) {
      autoProbeDone = true; void runProbe();
    }
    return status;
  }
  async function waitReady(epoch) {
    for (let attempt = 0; attempt < 60; attempt++) {
      const next = await refreshStatus();
      if (epoch !== operationEpoch) return false;
      if (next.ready) return true;
      if (next.operation?.error_code) throw Object.assign(new Error('Startup failed'), { code: next.operation.error_code });
      await new Promise(resolve => setTimeout(resolve, 500));
    }
    throw Object.assign(new Error('Startup timeout'), { code: 'engine_readiness_timeout' });
  }
  async function operation(kind, repairAction) {
    if (busy || probeBusy || status.operation?.busy) return;
    const epoch = ++operationEpoch; ++diagnosticEpoch; diagnosticsBusy = false; busy = true; localFailed = false; clearProblem(); clearProbe();
    $('localProblem').hidden = true;
    localPhase = kind === 'stop' ? 'Stopping safely…' : kind === 'local-trial' ? 'Preparing the local database…' : 'Starting the engine…'; render();
    try {
      const result = await request(`/admin/${kind}`, 'POST', repairAction ? { action: repairAction } : {});
      if (epoch !== operationEpoch) return;
      if (kind === 'stop') { await refreshStatus(); localPhase = 'Stopped'; return; }
      if (!await waitReady(epoch)) return;
      if (result?.access_token) await onTrial(result);
      if (epoch !== operationEpoch) return;
      localPhase = 'Ready';
      if (status.local_trial && canProbe()) { autoProbeDone = true; await runProbe(true); }
    } catch (error) {
      if (epoch !== operationEpoch) return;
      localFailed = true; localPhase = 'Startup needs attention';
      text('localProblem', problemText(error?.code)); $('localProblem').hidden = false;
      showProblem(error?.code, ['retry']);
    } finally { if (epoch === operationEpoch) { busy = false; render(); } }
  }
  async function runProbe(fromOperation = false) {
    if (probeBusy || (!fromOperation && busy) || !status.local_trial || !status.ready || !canProbe()) return;
    const epoch = ++probeEpoch; probeBusy = true; probeResult = null; autoProbeDone = true;
    for (const step of PROBE_STEPS) setStep(step, 'running');
    text('probeSummary', 'Testing the connection…'); render();
    try {
      const result = await request('/admin/probe', 'POST', {});
      if (epoch !== probeEpoch) return;
      const checks = new Map();
      for (const item of Array.isArray(result?.checks) ? result.checks : []) {
        if (PROBE_STEPS.includes(item?.step) && ['passed', 'failed', 'skipped'].includes(item.status) && !checks.has(item.step)) checks.set(item.step, item);
      }
      const ok = result?.ok === true && PROBE_STEPS.every(step => checks.get(step)?.status === 'passed');
      probeResult = { ok };
      for (const step of PROBE_STEPS) setStep(step, checks.get(step)?.status || 'skipped');
      text('probeSummary', ok ? 'Connection confirmed: HTTP, WebSocket, room entry, and initial synchronization passed.' : 'The connection test did not complete. Check setup, then try again.');
      if (!ok) showProblem([...checks.values()].find(item => item.status === 'failed')?.code);
      else { localFailed = false; $('localProblem').hidden = true; clearProblem(); }
    } catch (error) {
      if (epoch !== probeEpoch) return;
      probeResult = { ok: false }; for (const step of PROBE_STEPS) setStep(step, 'skipped');
      text('probeSummary', 'The connection test did not complete. Check setup, then try again.'); showProblem(error?.code);
    } finally {
      if (epoch === probeEpoch) {
        probeBusy = false;
        try { await refreshStatus(); } catch { /* The current check remains visible while launcher status is unavailable. */ }
        render();
      }
    }
  }
  async function diagnose() {
    if (diagnosticsBusy || busy) return;
    const epoch = ++diagnosticEpoch; diagnosticsBusy = true; render();
    try {
      const result = await request('/admin/diagnostics');
      if (epoch !== diagnosticEpoch) return;
      const checks = Array.isArray(result?.checks) ? result.checks : [];
      const failed = checks.find(item => item && ['failed', 'error', 'warning'].includes(item.status));
      const actions = (Array.isArray(result?.actions) ? result.actions : []).map(item => item?.action).filter(action => ['retry', 'choose_engine_port', 'choose_database_port'].includes(action));
      if (failed) showProblem(failed.code, actions);
      else if (checks.length > 0 && checks.every(item => item?.status === 'passed')) {
        clearProblem(); text('diagnosticSummary', 'Setup checks passed.'); $('diagnosticSummary').hidden = false;
      } else {
        clearProblem(); text('diagnosticSummary', checks.some(item => item?.code === 'operation_in_progress') ? problemText('operation_in_progress') : 'Setup checks did not complete. Check again.');
        $('diagnosticSummary').hidden = false; $('retryDiagnostics').hidden = false;
      }
    } catch (error) { if (epoch === diagnosticEpoch) showProblem(error?.code); }
    finally { if (epoch === diagnosticEpoch) { diagnosticsBusy = false; render(); } }
  }
  function bind(id, callback) { $(id).addEventListener('click', event => { event.preventDefault(); void callback(); }); }
  bind('startLocalTrial', () => operation('local-trial'));
  bind('retryLocalTrial', () => operation(status.initialized ? 'repair' : 'local-trial', status.initialized ? 'retry' : null));
  bind('startEngine', () => operation('start')); bind('stopEngine', () => operation('stop')); bind('restartEngine', () => operation('restart'));
  bind('runProbe', () => runProbe()); bind('retryProbe', () => runProbe());
  bind('runDiagnostics', diagnose); bind('retryDiagnostics', diagnose);
  bind('repairLocalStartup', () => operation('repair', 'retry')); bind('chooseEnginePort', () => operation('repair', 'choose_engine_port'));
  bind('chooseDatabasePort', () => operation('repair', 'choose_database_port'));
  bind('showManualSetup', showManual); bind('diagnosticManualSetup', showManual);
  return { observe, render, authChanged() { clearProbe(); observe(status); } };
}
