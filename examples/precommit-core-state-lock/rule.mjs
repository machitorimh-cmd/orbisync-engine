// Application rule only. Core knows no lock or application component names.
export const COMPONENT = 'org.example.document';
export function decide(request) {
  const deny = reason => ({ decision: 'deny', reason });
  const allow = { decision: 'allow' };
  if (!Object.hasOwn(request, 'current_entity')) return deny('Core state is required');
  const current = request.current_entity;
  const args = request.payload ?? {};
  if (request.operation === 'precommit.entity.spawn') {
    return current === null && args.locked !== true ? allow : deny('Spawn must start unlocked');
  }
  if (!current || current.revision !== request.client_expected_revision) return deny('State revision mismatch');
  const envelope = current.components?.[COMPONENT];
  if (envelope && envelope.encoding !== 'json') return deny('Unrecognized document encoding');
  const document = envelope?.value ?? {};
  const owner = request.requester === current.owner_id;
  if (document.locked === true) {
    return owner && request.operation === 'precommit.entity.update'
      && request.component_key === COMPONENT && args.locked === false
      ? allow : deny('Only the owner can unlock the document');
  }
  if (request.operation === 'precommit.entity.update' && request.component_key === COMPONENT
      && args.locked === true && !owner) return deny('Only the owner can lock the document');
  return allow;
}
