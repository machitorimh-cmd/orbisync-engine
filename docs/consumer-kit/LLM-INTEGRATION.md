# Integration brief for another LLM

You receive this kit and an operator-provided endpoint/contract, not engine source.
Read README.md first. The game and implementation languages are chosen by the
consumer; do not tailor integration to a particular existing game or example.
For TypeScript/JavaScript, install the local SDK tarball and use public exports
from `@orbisync/client`; use its declarations and do not reimplement its session
machinery. For another language, read CLIENT-WIRE.md and use the bundled
`protocol/orbisync/v1/realtime.proto` and `protocol/http/orbisync-v1.yaml` to
implement encoding, authentication, session, canonical state and input receipts
with that language's libraries. Generate bindings as needed from these public
schemas. Do not import engine repository paths. Language-independent protocol
availability does not mean an SDK already exists for every language.

Ask the operator for API base URL, instance UUID, enabled auth method, secure
credential acquisition, owned/visible entity IDs or authorized provisioning flow,
and rule name + intent/canonical component schemas. Keep those values separate
from game UI/rendering. Do not invent server endpoints or assume a world ID is an
instance ID. Read EXTERNAL-RULES.md before implementing new rules. Build three
parts with clear responsibilities: frontend client integration, a separately hosted
application HTTPS JSON rule service, and operator configuration of the stock
engine. Start from external-input-python/service.py or implement the same signed
v1 contract in your chosen language. Produce the intent/component schemas and
input-rules.json for the operator, who supplies real world IDs, endpoints and
secrets and restarts the engine with --input-rules. Do not modify the engine or
claim a frontend can register rules. Rules compute proposals only; no irreversible
side effects, commit claims or trusting client-supplied outcome fields. Read the
canonical state and authenticated context supplied in each signed request.
The Python service demonstrates this HTTP contract only; use any suitable
language for the service, independently of the frontend language. The gameplay
examples below use the available TypeScript SDK; preserve their lifecycle and
canonical-state requirements when implementing another-language client.

Read [Application numbers and integer fields](EXTERNAL-RULES.md#application-numbers-and-integer-fields)
and [CLIENT-WIRE.md](CLIENT-WIRE.md) before validating application numbers.
Protobuf Struct numbers are binary64: integer-valued intent **and canonical
state**, including application version fields, may arrive as `0.0` or `1.0`.
Validate finite numeric value, integrality and field range, then normalize;
do not require exact Python `int` type or integer JSON spelling. Reject booleans,
fractions and out-of-range values for integer fields; use the documented safe
integer range and agreed strings for larger integers. Test both intent and
canonical state with integral floats and invalid values. Do not impose integer
constraints on fields whose application schema permits fractions.

For a **new game**, build a small scene with a loading/error/reconnect indicator,
connectSession, canonical entity rendering, input disabled unless ready, and one
PredictedInput controller per controlled entity. The supplied CLI/movementView is
usable as-is only if the operator provisions example.move/example.position. Use
the game's own pure read/predict functions for other provisioned contracts.

For an **existing game**, retain its render loop and domain model. Add an adapter
mapping canonical entity UUIDs/components into that model and player actions into
the provisioned intent schema. Identify which fields are server authoritative;
do not run competing local writes to those fields. Use prediction only for display,
and interpolate remote state in the existing render loop. Handle entity removal,
snapshots, reconnect and teardown within existing scene lifecycle hooks.

Keep authoritative and predicted display state distinct. Wait for receipts before
the next controller submission. Accepted/rejected/uncertain are different outcomes.
Preserve uncertain request ID/revision/intent; retry explicitly only after ready
and according to game policy. A timeout does not mean rollback. Canonical adoption
does not cancel an in-flight server command. Dispose controllers/views and await
session.leave on shutdown. Never log credentials or tokens.

Verify installation, typecheck/build, then connect/auth/join, accepted and rejected
intent, canonical reflection, reconnect and cleanup against the supplied endpoint
using an operator-approved test identity/entity. Report what ran and distinguish
fixture evidence from production/browser acceptance. Do not claim deployment of
new server rules or production readiness from a CLI fixture check.
