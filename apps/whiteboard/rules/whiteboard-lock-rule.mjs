// Application rule for the whiteboard demo. Core knows nothing about notes or
// locks: it only forwards the state it observed. The lock正本 is the `locked`
// field of the note component inside `current_entity`, never an external store
// and never anything the client puts in `payload`.
export const COMPONENT = 'com.orbisync.whiteboard.note';

/**
 * Decides a single pre-commit request payload (the `payload` object of the
 * signed webhook body). Pure: no I/O, no stored reservations, no TTL.
 */
export function decide(request) {
  const deny = reason => ({ decision: 'deny', reason });
  const allow = { decision: 'allow' };
  // A Core that does not send the field at all cannot be enforced against.
  if (!request || typeof request !== 'object') return deny('判定要求が不正です');
  if (!Object.hasOwn(request, 'current_entity')) return deny('Coreの現在状態が必要です');
  const current = request.current_entity;
  const args = request.payload ?? {};
  if (request.operation === 'precommit.entity.spawn') {
    return current === null && args.locked !== true
      ? allow
      : deny('新しい付箋は未ロックで作成してください');
  }
  if (!current || current.revision !== request.client_expected_revision) {
    return deny('付箋の状態が変化しています');
  }
  const envelope = current.components?.[COMPONENT];
  if (envelope && envelope.encoding !== 'json') return deny('付箋componentの形式が不正です');
  const note = envelope?.value ?? {};
  const owner = request.requester === current.owner_id;
  const noteUpdate = request.operation === 'precommit.entity.update'
    && request.component_key === COMPONENT;
  if (note.locked === true) {
    // Locked means frozen: the owner's only permitted command is the unlock,
    // which carries the whole component because Core replaces it wholesale.
    return owner && noteUpdate && args.locked === false
      ? allow
      : deny('ロック中の付箋は所有者の解除だけを受け付けます');
  }
  if (noteUpdate && args.locked === true && !owner) {
    return deny('付箋をロックできるのは所有者だけです');
  }
  return allow;
}
