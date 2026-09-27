fn mailbox_rejection(error: MailboxSendError) -> CommandOutcome {
    let (code, detail) = match error {
        MailboxSendError::Full { lane } => (
            "MAILBOX_SATURATED",
            format!("instance mailbox {lane:?} queue is full"),
        ),
        MailboxSendError::Closed { lane } => (
            "MAILBOX_CLOSED",
            format!("instance mailbox {lane:?} queue is closed"),
        ),
    };
    CommandOutcome::Rejected { code, detail }
}

// ---------------------------------------------------------------------------
// EntityCommand helpers (W-16, D-11)
// ---------------------------------------------------------------------------

/// Converts a `prost_types::Value` into a `serde_json::Value`.
///
/// Used to serialize `google.protobuf.Struct` arguments into deterministic
/// JSON bytes for `UpdateEntityComponent::payload_bytes` (D-11). Keys are
/// sorted via `BTreeMap` at the `Struct` level so the same logical payload
/// yields the same bytes regardless of `HashMap` iteration order.
fn prost_value_to_json(value: &prost_types::Value) -> serde_json::Value {
    use prost_types::value::Kind;
    match &value.kind {
        Some(Kind::NullValue(_)) => serde_json::Value::Null,
        Some(Kind::NumberValue(n)) => serde_json::Number::from_f64(*n)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Some(Kind::StringValue(s)) => serde_json::Value::String(s.clone()),
        Some(Kind::BoolValue(b)) => serde_json::Value::Bool(*b),
        Some(Kind::StructValue(s)) => {
            let map: std::collections::BTreeMap<String, serde_json::Value> = s
                .fields
                .iter()
                .map(|(k, v)| (k.clone(), prost_value_to_json(v)))
                .collect();
            serde_json::Value::Object(map.into_iter().collect())
        }
        Some(Kind::ListValue(l)) => {
            serde_json::Value::Array(l.values.iter().map(prost_value_to_json).collect())
        }
        None => serde_json::Value::Null,
    }
}

/// Serializes `prost_types::Struct` to JSON bytes with dictionary-sorted keys.
///
/// D-11 requires sorting because `prost` generates `HashMap` for `fields`
/// which has non-deterministic iteration. `payload_bytes` participates in
/// revision comparison, so non-determinism would cause spurious conflicts.
fn struct_to_sorted_json_bytes(s: &prost_types::Struct) -> Vec<u8> {
    serde_json::to_vec(&struct_to_sorted_json_value(s)).unwrap_or_default()
}

/// Converts `prost_types::Struct` to a dictionary-sorted `serde_json::Value`.
///
/// Used by the pre-commit validation hook (ADR-025) to forward the client's
/// `arguments` to an extension without exposing Core's internal wire types.
pub(crate) fn struct_to_sorted_json_value(s: &prost_types::Struct) -> serde_json::Value {
    let map: std::collections::BTreeMap<String, serde_json::Value> = s
        .fields
        .iter()
        .map(|(k, v)| (k.clone(), prost_value_to_json(v)))
        .collect();
    serde_json::Value::Object(map.into_iter().collect())
}

fn extract_string_field(s: &prost_types::Struct, key: &str) -> Option<String> {
    s.fields.get(key).and_then(|v| match &v.kind {
        Some(prost_types::value::Kind::StringValue(s)) => Some(s.clone()),
        _ => None,
    })
}

fn parse_transfer_owner(s: &prost_types::Struct) -> Result<UserId, String> {
    if s.fields.len() != 1 || !s.fields.contains_key("new_owner_id") {
        return Err("transfer_ownership requires exactly one new_owner_id field".to_owned());
    }
    let raw = extract_string_field(s, "new_owner_id")
        .ok_or_else(|| "new_owner_id is missing or not a string".to_owned())?;
    UserId::parse(&raw).map_err(|_| "new_owner_id is not a valid canonical UUIDv7".to_owned())
}

fn extract_number_field(s: &prost_types::Struct, key: &str) -> Option<f64> {
    s.fields.get(key).and_then(|v| match &v.kind {
        Some(prost_types::value::Kind::NumberValue(n)) => Some(*n),
        _ => None,
    })
}

fn parse_entity_kind(s: &prost_types::Struct) -> Result<EntityKind, String> {
    let raw = extract_string_field(s, "kind")
        .ok_or_else(|| "kind field missing or not a string".to_owned())?;
    EntityKind::parse(raw.to_lowercase().as_str()).map_err(|e| e.to_string())
}

fn parse_visibility(s: &prost_types::Struct) -> Result<VisibilityPolicy, String> {
    let raw = extract_string_field(s, "visibility")
        .ok_or_else(|| "visibility field missing or not a string".to_owned())?;
    match raw.to_lowercase().as_str() {
        "global" => Ok(VisibilityPolicy::Global),
        "owner_only" | "owneronly" | "owner-only" => Ok(VisibilityPolicy::OwnerOnly),
        "spatial" => {
            let radius = extract_number_field(s, "visibility_radius")
                .or_else(|| extract_number_field(s, "radius"))
                .or_else(|| extract_number_field(s, "spatial_radius"))
                .ok_or_else(|| "spatial visibility requires visibility_radius number".to_owned())?;
            VisibilityPolicy::spatial(radius as f32).map_err(|e| e.to_string())
        }
        other => Err(format!("unknown visibility: {other}")),
    }
}

fn parse_transform_from_struct(s: &prost_types::Struct) -> Result<Option<Transform>, String> {
    // Prefer nested "transform" Struct if present.
    if let Some(v) = s.fields.get("transform") {
        if let Some(prost_types::value::Kind::StructValue(inner)) = &v.kind {
            let x = extract_number_field(inner, "position_x");
            let y = extract_number_field(inner, "position_y");
            let z = extract_number_field(inner, "position_z");
            if x.is_some() || y.is_some() || z.is_some() {
                let px = x.unwrap_or(0.0);
                let py = y.unwrap_or(0.0);
                let pz = z.unwrap_or(0.0);
                let pos = Vec3::new(px, py, pz).map_err(|e| e.to_string())?;
                let rot = orbisync_domain::Quaternion::new(0.0, 0.0, 0.0, 1.0)
                    .map_err(|e| e.to_string())?;
                let scale = Vec3::new(1.0, 1.0, 1.0).map_err(|e| e.to_string())?;
                let t = Transform::new(pos, rot, scale).map_err(|e| e.to_string())?;
                return Ok(Some(t));
            }
            // No position in nested struct -> no transform
            return Ok(None);
        }
    }
    // Top-level position_x/y/z
    let x = extract_number_field(s, "position_x");
    let y = extract_number_field(s, "position_y");
    let z = extract_number_field(s, "position_z");
    if x.is_some() || y.is_some() || z.is_some() {
        let px = x.unwrap_or(0.0);
        let py = y.unwrap_or(0.0);
        let pz = z.unwrap_or(0.0);
        let pos = Vec3::new(px, py, pz).map_err(|e| e.to_string())?;
        let rot =
            orbisync_domain::Quaternion::new(0.0, 0.0, 0.0, 1.0).map_err(|e| e.to_string())?;
        let scale = Vec3::new(1.0, 1.0, 1.0).map_err(|e| e.to_string())?;
        let t = Transform::new(pos, rot, scale).map_err(|e| e.to_string())?;
        return Ok(Some(t));
    }
    Ok(None)
}

/// Broadcasts an `EntityCommand` as a reliable event (W-16, D-15).
///
/// Unlike `StateDelta` (latest-wins, may be dropped on `try_send` overflow),
/// `EntityCommand` is a reliable event: `delivery.rs` must not drop it on
/// overflow. Since B-3 this is enforced – `DeliveryRegistry::broadcast` takes a
/// `Reliability` and disconnects a slow consumer instead of dropping a reliable
/// payload. `OutboundQueue` (W-17) will later classify lanes per connection. The envelope is sent with `sequence: 0` so each receiver's
/// `filter_payload_for_viewer` can rewrite it to its per-connection
/// `next_sequence` after interest filtering (M-7).
async fn broadcast_entity_command(
    state: &RealtimeState,
    instance_id: InstanceId,
    command: orbisync_protocol::v1::EntityCommand,
    message_id: String,
    negotiated_minor: u32,
    views: Arc<orbisync_world_runtime::InterestSnapshot>,
    revision: u64,
) {
    let env = Envelope {
        protocol_major: orbisync_protocol::PROTOCOL_MAJOR,
        protocol_minor: negotiated_minor,
        message_id,
        sequence: 0,
        sent_at_unix_ms: state.clock.now().to_unix_millis().unwrap_or(0),
        instance_id: instance_id.to_string(),
        payload: Some(envelope::Payload::EntityCommand(command)),
    };
    let mut buf = Vec::new();
    if env.encode(&mut buf).is_ok() {
        if retain_reliable_event(state, instance_id, revision, buf.clone()).await.is_err() {
            if state.registry.contains(instance_id) {
                tracing::error!(event = "realtime.history_unavailable", instance_id = %instance_id);
                return;
            }
            // Durability recovery can finish after the actor has been reaped.
            // Its saved command remains deduplicated; deliver that result to
            // currently connected sinks even though no actor can retain replay.
            tracing::warn!(event = "realtime.history_actor_stopped", instance_id = %instance_id);
        }
        // B-3: EntityCommand is reliable. Overflow must not be dropped silently;
        // `broadcast` disconnects the slow consumer and emits a warning instead.
        let outcome =
            state.delivery.broadcast_with_views(instance_id, buf, Reliability::Reliable, views, state.interest_grid);
        if outcome.disconnected_slow_consumers > 0 {
            tracing::warn!(
                event = "realtime.entity_command_slow_consumer",
                instance_id = %instance_id,
                disconnected = outcome.disconnected_slow_consumers,
                "disconnected slow consumers rather than dropping a reliable EntityCommand"
            );
        }
    }
}

/// Sends a retained successful command result directly to a duplicate
/// requester.  This is an idempotent acknowledgement, not a second
/// broadcast; its client command ID remains unchanged.
async fn send_command_result(
    socket: &mut RealtimeSocket,
    result: &crate::command_dedup::CommandDedupResult,
    request_message_id: String,
    negotiated_minor: u32,
    sequence: u64,
    clock: &dyn Clock,
    instance_id: &str,
) {
    match result {
        crate::command_dedup::CommandDedupResult::Applied {
            command,
            message_id,
        } => {
            let env = Envelope {
                protocol_major: orbisync_protocol::PROTOCOL_MAJOR,
                protocol_minor: negotiated_minor,
                message_id: message_id.clone(),
                sequence,
                sent_at_unix_ms: clock.now().to_unix_millis().unwrap_or(0),
                instance_id: instance_id.to_owned(),
                payload: Some(envelope::Payload::EntityCommand(command.clone())),
            };
            let mut buf = Vec::new();
            if env.encode(&mut buf).is_ok() {
                // Reason: best-effort send on a duplicate request's socket;
                // failure means the peer disconnected and is not actionable.
                #[allow(clippy::let_underscore_must_use)]
                let _ = socket.write(Message::Binary(buf.into())).await;
            }
        }
        crate::command_dedup::CommandDedupResult::Rejected {
            code,
            detail,
            message_id,
        } => {
            send_error_with_message_id(
                socket,
                code,
                detail.clone(),
                request_message_id,
                message_id.clone(),
                negotiated_minor,
                sequence,
                clock,
                instance_id,
                false,
            )
            .await;
        }
    }
}

/// Builds an interest-filtered snapshot payload for `viewer_pos`.
///
/// Uses `interest_filter::filter_visible_views` (N-5) with the provided
/// `grid` (config-driven, H-6c). The actor's cached `Arc<orbisync_world_runtime::InterestSnapshot>`
/// is cloned in O(1) instead of deep-cloning `Entity` with its `components`
/// map per receiver per message. When `viewer_pos` is `None`, all entities
/// are included per helper fallback. The resulting JSON contains the complete
/// public join state and only the visible entities. It is marked `filtered:true`
/// so filtering is observable to clients.
///
/// `subscribed` tracks per-entity hysteresis state (N-3). When `viewer_pos` is `None`,
/// the snapshot is returned unfiltered and `subscribed` is not modified except for
/// pruning stale entries. Otherwise the filtered result is recorded so that
/// subsequent delta filtering can apply hysteresis (near 30 m / unsubscribe 35 m)
/// without flapping. Pruning reuses the snapshot's shared membership index.
struct SnapshotContext<'a> {
    instance: &'a orbisync_domain::WorldInstance,
    world: &'a orbisync_domain::World,
    metrics: &'a dyn MetricsRecorder,
    permissions: WorldPermissions,
    server_time_unix_ms: i64,
}

trait RuntimeReadView {
    fn interest_views(&self) -> Arc<orbisync_world_runtime::InterestSnapshot>;
    fn entities_snapshot(&self) -> Vec<orbisync_domain::Entity>;
    fn members_snapshot(&self) -> Vec<(PresenceId, UserId)>;
    fn revision(&self) -> orbisync_domain::Revision;
}

/// Converts validated custom components into the wire-level properties shape.
/// Component keys are retained so clients can select their own application
/// component. The reserved `core.*` namespace is server-owned and omitted.
pub(crate) fn public_component_properties(
    entity: &orbisync_domain::Entity,
) -> Option<prost_types::Struct> {
    let fields: std::collections::BTreeMap<String, prost_types::Value> = entity
        .components()
        .iter()
        .filter_map(|(component_key, payload)| {
            if component_key.split_once('.')?.0 == "core" {
                return None;
            }
            let (encoding, value) = match serde_json::from_slice::<serde_json::Value>(payload) {
                Ok(value) if json_fits_struct(&value) => ("json", serde_json_to_prost_value(value)?),
                _ => (
                    "base64",
                    prost_types::Value {
                        kind: Some(prost_types::value::Kind::StringValue(
                            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, payload),
                        )),
                    },
                ),
            };
            let envelope = prost_types::Struct {
                fields: std::collections::BTreeMap::from([
                    (
                        "encoding".to_owned(),
                        prost_types::Value {
                            kind: Some(prost_types::value::Kind::StringValue(
                                encoding.to_owned(),
                            )),
                        },
                    ),
                    ("value".to_owned(), value),
                ]),
            };
            Some((
                component_key.clone(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::StructValue(
                        envelope,
                    )),
                },
            ))
        })
        .collect();
    if fields.is_empty() {
        return None;
    }
    Some(prost_types::Struct { fields })
}

/// Convert only the entity owned by the mutation outcome, never a later read.
fn committed_delta_entity(entity: &orbisync_domain::Entity) -> orbisync_protocol::v1::EntityState {
    let transform = entity.transform().map(|transform| {
        let position = transform.position();
        let rotation = transform.rotation();
        orbisync_protocol::v1::Transform {
            position_x: position.x(), position_y: position.y(), position_z: position.z(),
            rotation_x: rotation.x(), rotation_y: rotation.y(), rotation_z: rotation.z(), rotation_w: rotation.w(),
        }
    });
    orbisync_protocol::v1::EntityState {
        entity_id: entity.id().to_string(), revision: entity.revision().as_u64(), transform,
        properties: Some(public_component_properties(entity).unwrap_or_default()),
        velocity: None, animation: None, presence: None,
    }
}

/// Struct's f64 numbers cannot represent arbitrary JSON integers. Preserve the
/// original component bytes through the existing base64 envelope in that case.
pub(crate) fn json_fits_struct(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Number(number) => number.as_f64().is_some_and(|value| value.is_finite() && value.abs() <= 9_007_199_254_740_991.0),
        serde_json::Value::Array(values) => values.iter().all(json_fits_struct),
        serde_json::Value::Object(values) => values.values().all(json_fits_struct),
        _ => true,
    }
}

pub(crate) fn serde_json_to_prost_value(value: serde_json::Value) -> Option<prost_types::Value> {
    use prost_types::value::Kind;
    let kind = match value {
        serde_json::Value::Null => Kind::NullValue(0),
        serde_json::Value::Bool(value) => Kind::BoolValue(value),
        serde_json::Value::Number(value) => Kind::NumberValue(value.as_f64()?),
        serde_json::Value::String(value) => Kind::StringValue(value),
        serde_json::Value::Array(values) => Kind::ListValue(prost_types::ListValue {
            values: values
                .into_iter()
                .filter_map(serde_json_to_prost_value)
                .collect(),
        }),
        serde_json::Value::Object(fields) => Kind::StructValue(prost_types::Struct {
            fields: fields
                .into_iter()
                .filter_map(|(key, value)| {
                    serde_json_to_prost_value(value).map(|value| (key, value))
                })
                .collect(),
        }),
    };
    Some(prost_types::Value { kind: Some(kind) })
}

fn public_component_properties_json(entity: &orbisync_domain::Entity) -> serde_json::Value {
    public_component_properties(entity)
        .map(|properties| {
            let fields = properties
                .fields
                .into_iter()
                .map(|(key, value)| (key, prost_value_to_json(&value)))
                .collect::<serde_json::Map<_, _>>();
            serde_json::Value::Object(fields)
        })
        .unwrap_or(serde_json::Value::Null)
}


impl RuntimeReadView for InstanceActor {
    fn interest_views(&self) -> Arc<orbisync_world_runtime::InterestSnapshot> {
        InstanceActor::interest_views(self)
    }

    fn entities_snapshot(&self) -> Vec<orbisync_domain::Entity> {
        InstanceActor::entities_snapshot(self)
    }

    fn members_snapshot(&self) -> Vec<(PresenceId, UserId)> {
        InstanceActor::members_snapshot(self)
    }

    fn revision(&self) -> orbisync_domain::Revision {
        InstanceActor::revision(self)
    }
}

impl RuntimeReadView for orbisync_world_runtime::InstanceReadSnapshot {
    fn interest_views(&self) -> Arc<orbisync_world_runtime::InterestSnapshot> {
        Arc::clone(&self.interest_views)
    }

    fn entities_snapshot(&self) -> Vec<orbisync_domain::Entity> {
        self.entities.clone()
    }

    fn members_snapshot(&self) -> Vec<(PresenceId, UserId)> {
        self.members.clone()
    }

    fn revision(&self) -> orbisync_domain::Revision {
        self.revision
    }
}

/// Returns the entity and opaque presence data for `JoinAccepted` after the
/// same interest pass used by the initial snapshot.
pub(crate) fn filtered_join_accepted_state(
    read: &orbisync_world_runtime::InstanceReadSnapshot,
    viewer_pos: Vec3,
    viewer_user: UserId,
    grid: &orbisync_interest::UniformGrid,
    subscribed: &mut std::collections::HashMap<EntityId, bool>,
    viewer_roles: Option<&std::collections::BTreeSet<orbisync_domain::RoleId>>,
) -> (
    Vec<orbisync_protocol::v1::EntityState>,
    Vec<String>,
    Vec<String>,
) {
    let visible_ids = crate::interest_filter::filter_visible_views_with_roles(
        Some(viewer_pos),
        read.interest_views.iter(),
        grid,
        Some(viewer_user),
        viewer_roles,
        Some(&*subscribed),
    );
    let visible_ids: std::collections::HashSet<EntityId> = visible_ids.into_iter().collect();
    let mut entities = read
        .entities
        .iter()
        .filter(|entity| visible_ids.contains(&entity.id()))
        .collect::<Vec<_>>();
    entities.sort_by_key(|entity| entity.id().to_string());

    let visible_owner_users: std::collections::HashSet<UserId> = entities
        .iter()
        .filter_map(|entity| entity.owner())
        .collect();
    let nearby_entities = entities
        .iter()
        .map(|entity| {
            let transform = entity.transform().map(|value| {
                let position = value.position();
                let rotation = value.rotation();
                orbisync_protocol::v1::Transform {
                    position_x: position.x(),
                    position_y: position.y(),
                    position_z: position.z(),
                    rotation_x: rotation.x(),
                    rotation_y: rotation.y(),
                    rotation_z: rotation.z(),
                    rotation_w: rotation.w(),
                }
            });
            orbisync_protocol::v1::EntityState {
                entity_id: entity.id().to_string(),
                revision: entity.revision().as_u64(),
                transform,
                velocity: entity.velocity().map(|value| orbisync_protocol::v1::Velocity {
                    x: value.x(), y: value.y(), z: value.z(),
                }),
                animation: entity.animation().map(|value| orbisync_protocol::v1::Animation {
                    clip: value.clip().to_owned(), time: value.time(), speed: value.speed(), looping: value.looping(),
                }),
                presence: entity.presence().map(|value| orbisync_protocol::v1::Presence {
                    state: value.state().as_str().to_owned(), last_seen: value.last_seen(),
                }),
                properties: public_component_properties(entity),
            }
        })
        .collect();

    let mut nearby_users = read
        .members
        .iter()
        .filter(|(_, member_user)| {
            *member_user == viewer_user || visible_owner_users.contains(member_user)
        })
        .map(|(presence, member_user)| (presence.to_string(), member_user.to_string()))
        .collect::<Vec<_>>();
    nearby_users.sort_by(|left, right| left.0.cmp(&right.0));
    let (nearby_presence_ids, nearby_user_ids) = nearby_users.into_iter().unzip();

    (nearby_entities, nearby_presence_ids, nearby_user_ids)
}

#[allow(dead_code)]
fn filtered_snapshot_for_viewer(
    actor: &impl RuntimeReadView,
    viewer_pos: Option<Vec3>,
    viewer_user: Option<UserId>,
    grid: &orbisync_interest::UniformGrid,
    subscribed: &mut std::collections::HashMap<EntityId, bool>,
    context: SnapshotContext<'_>,
) -> Vec<u8> {
    filtered_snapshot_for_viewer_with_roles(
        actor,
        viewer_pos,
        viewer_user,
        grid,
        subscribed,
        None,
        context,
    )
}

#[tracing::instrument(
    name = "instance_runtime.snapshot_generation",
    skip_all,
    fields(instance_id = tracing::field::Empty)
)]
fn filtered_snapshot_for_viewer_with_roles(
    actor: &impl RuntimeReadView,
    viewer_pos: Option<Vec3>,
    viewer_user: Option<UserId>,
    grid: &orbisync_interest::UniformGrid,
    subscribed: &mut std::collections::HashMap<EntityId, bool>,
    viewer_roles: Option<&std::collections::BTreeSet<orbisync_domain::RoleId>>,
    context: SnapshotContext<'_>,
) -> Vec<u8> {
    tracing::Span::current().record(
        "instance_id",
        tracing::field::display(context.instance.id()),
    );
    let views = actor.interest_views();
    let visible = crate::interest_filter::filter_visible_views_with_roles(
        viewer_pos,
        views.iter(),
        grid,
        viewer_user,
        viewer_roles,
        Some(&*subscribed),
    );
    context
        .metrics
        .observe(Histogram::InterestVisibleSetSize, visible.len() as f64);
    let visible_ids: std::collections::HashSet<EntityId> = visible.into_iter().collect();
    // Record initial hysteresis state for this viewer (N-3).
    for v in views.iter() {
        let is_visible = visible_ids.contains(&v.id);
        subscribed.insert(v.id, is_visible);
    }
    subscribed.retain(|id, _| views.contains(*id));
    let all_entities = actor.entities_snapshot();
    let mut entities = all_entities
        .into_iter()
        .filter(|entity| visible_ids.contains(&entity.id()))
        .collect::<Vec<_>>();
    // Runtime state is stored in hash maps. Stable ordering keeps chunk bytes
    // and snapshot tests deterministic without changing visibility semantics.
    entities.sort_by_key(|entity| entity.id().to_string());
    let visible_owner_users: std::collections::HashSet<UserId> = entities
        .iter()
        .filter_map(|entity| entity.owner())
        .collect();

    let entity_values = entities
        .into_iter()
        .map(|entity| {
            let transform = entity.transform().map(|value| {
                let position = value.position();
                let rotation = value.rotation();
                let scale = value.scale();
                serde_json::json!({
                    "position": {"x": position.x(), "y": position.y(), "z": position.z()},
                    "rotation": {"x": rotation.x(), "y": rotation.y(), "z": rotation.z(), "w": rotation.w()},
                    "scale": {"x": scale.x(), "y": scale.y(), "z": scale.z()},
                })
            });
            serde_json::json!({
                "entity_id": entity.id().to_string(),
                "revision": entity.revision().as_u64(),
                "kind": entity.kind().as_str(),
                "owner_user_id": entity.owner().map(|owner| owner.to_string()),
                "components": entity.components().iter().collect::<std::collections::BTreeMap<_, _>>(),
                "transform": transform,
                "velocity": entity.velocity().map(|velocity| {
                    serde_json::json!({
                        "x": velocity.x(),
                        "y": velocity.y(),
                        "z": velocity.z(),
                    })
                }),
                "properties": public_component_properties_json(&entity),
                "animation": entity.animation().map(|value| serde_json::json!({
                    "clip": value.clip(), "time": value.time(), "speed": value.speed(), "looping": value.looping(),
                })),
                "presence": entity.presence().map(|value| serde_json::json!({
                    "state": value.state().as_str(), "last_seen": value.last_seen(),
                })),
            })
        })
        .collect::<Vec<_>>();

    let mut presence_ids = actor
        .members_snapshot()
        .into_iter()
        .filter(|(_, member_user)| {
            viewer_user == Some(*member_user) || visible_owner_users.contains(member_user)
        })
        .map(|(presence, _)| presence)
        .collect::<Vec<_>>();
    presence_ids.sort_by_key(ToString::to_string);
    let presence_values = presence_ids
        .into_iter()
        .map(|id| serde_json::json!({"presence_id": id.to_string()}))
        .collect::<Vec<_>>();

    serde_json::to_vec(&serde_json::json!({
        "format": "orbisync.snapshot.v1",
        "user": {"user_id": viewer_user.map_or_else(String::new, |id| id.to_string())},
        "instance": {
            "instance_id": context.instance.id().to_string(),
            "world_id": context.instance.world_id().to_string(),
            "capacity": context.instance.capacity(),
            "lifecycle": context.instance.lifecycle().as_str(),
            "world_name": context.world.name(),
        },
        "permissions": {
            "entity_spawn": context.permissions.entity_spawn,
            "entity_update_own": context.permissions.entity_update_own,
            "entity_update_any": context.permissions.entity_update_any,
        },
        "entities": entity_values,
        "users": presence_values,
        "revision": actor.revision().as_u64(),
        "server_time_unix_ms": context.server_time_unix_ms,
        "filtered": true,
    }))
    .unwrap_or_else(|_| br"{}".to_vec())
}

/// Snapshot chunks use the protocol's 16 KiB reliable-bulk limit.
pub const SNAPSHOT_CHUNK_BYTES: usize = 16 * 1024;

/// Keep the boundary and serialized state tied to the same immutable read.
#[allow(clippy::too_many_arguments)]
fn prepare_snapshot_for_viewer(
    read: &impl RuntimeReadView,
    viewer_pos: Option<Vec3>,
    viewer_user: Option<UserId>,
    grid: &orbisync_interest::UniformGrid,
    subscribed: &mut std::collections::HashMap<EntityId, bool>,
    roles: Option<&std::collections::BTreeSet<orbisync_domain::RoleId>>,
    context: SnapshotContext<'_>,
) -> (u64, Vec<u8>) {
    let data = filtered_snapshot_for_viewer_with_roles(
        read, viewer_pos, viewer_user, grid, subscribed, roles, context,
    );
    (read.revision().as_u64(), data)
}

/// Splits snapshot data into at least one chunk and preserves byte boundaries.
#[must_use]
fn snapshot_chunks(data: &[u8]) -> Vec<Vec<u8>> {
    if data.is_empty() {
        return vec![Vec::new()];
    }
    data.chunks(SNAPSHOT_CHUNK_BYTES)
        .map(<[u8]>::to_vec)
        .collect()
}

/// Builds protocol payloads for one logical snapshot.
///
/// Keeping this metadata assembly pure makes the reliable-bulk contract
/// directly testable without opening a WebSocket.
#[must_use]
fn snapshot_payloads(
    snapshot_id: &str,
    data: &[u8],
    instance_revision: u64,
) -> Vec<orbisync_protocol::v1::Snapshot> {
    let chunks = snapshot_chunks(data);
    let chunk_count = u32::try_from(chunks.len()).unwrap_or(u32::MAX);
    chunks
        .into_iter()
        .enumerate()
        .map(|(chunk_index, data)| orbisync_protocol::v1::Snapshot {
            snapshot_id: snapshot_id.to_owned(),
            chunk_index: u32::try_from(chunk_index).unwrap_or(u32::MAX),
            chunk_count,
            instance_revision,
            data,
        })
        .collect()
}

/// Sends all chunks of one logical snapshot with a shared snapshot ID.
fn record_snapshot_bytes(metrics: &dyn MetricsRecorder, bytes: usize) {
    metrics.add(
        Counter::SnapshotBytesTotal,
        u64::try_from(bytes).unwrap_or(u64::MAX),
    );
}

async fn send_snapshot_chunks(
    socket: &mut RealtimeSocket,
    snapshot: (u64, Vec<u8>),
    instance_id: InstanceId,
    negotiated_minor: u32,
    first_sequence: u64,
    clock: &dyn Clock,
    metrics: &dyn MetricsRecorder,
    delivery_sink: &RealtimeDeliverySink,
) -> u64 {
    let (instance_revision, data) = snapshot;
    // Count the complete logical snapshot once, not each 16 KiB chunk.
    record_snapshot_bytes(metrics, data.len());
    let snapshot_id = uuid::Uuid::now_v7().to_string();
    let snapshots = snapshot_payloads(&snapshot_id, &data, instance_revision);
    let snapshot_count = u64::try_from(snapshots.len()).unwrap_or(u64::MAX);
    for (chunk_index, snapshot) in snapshots.into_iter().enumerate() {
        if delivery_sink.is_closed() {
            // Never complete a snapshot after losing a handoff update.
            #[allow(clippy::let_underscore_must_use)]
            let _ = socket.write(Message::Close(None)).await;
            return first_sequence.saturating_add(chunk_index as u64);
        }
        let envelope = Envelope {
            protocol_major: orbisync_protocol::PROTOCOL_MAJOR,
            protocol_minor: negotiated_minor,
            message_id: uuid::Uuid::now_v7().to_string(),
            sequence: first_sequence.saturating_add(chunk_index as u64),
            sent_at_unix_ms: clock.now().to_unix_millis().unwrap_or(0),
            instance_id: instance_id.to_string(),
            payload: Some(envelope::Payload::Snapshot(snapshot)),
        };
        let mut buf = Vec::new();
        if envelope.encode(&mut buf).is_err() {
            // Reason: best-effort close after an impossible local encoding
            // failure; the peer cannot consume a malformed snapshot chunk.
            #[allow(clippy::let_underscore_must_use)]
            let _ = socket.write(Message::Close(None)).await;
            break;
        }
        // Reason: best-effort send on a socket that may already be closed;
        // a send failure is handled by the connection teardown path.
        #[allow(clippy::let_underscore_must_use)]
        let _ = socket.write(Message::Binary(buf.into())).await;
    }
    first_sequence.saturating_add(snapshot_count)
}

/// Filters a broadcast `payload` (expected `Envelope::StateDelta`) to entities visible to `viewer_pos`.
///
/// Uses `interest_filter::filter_visible_views` (N-5) with the provided `grid` (H-6)
/// so snapshot and delta share the same interest logic (hysteresis + 5×5 cells +
/// `VisibilityPolicy`). No hardcoded 35. The authoritative position/policy is
/// taken from `views` (`&[EntityInterestView]`) which is an `Arc`-cloned
/// lightweight snapshot, avoiding `Entity.components` deep clone per receiver
/// per message. When the entity is not in `views`, falls back to distance +
/// cell check using `grid` (no policy).
///
/// - When `viewer_pos` is `None`, the payload is returned unchanged (include all).
/// - For `StateDelta`, each `EntityState` is matched to its `EntityInterestView`.
/// - If the filtered delta has zero entities, `None` is returned (drop, not forwarded).
/// - Non-delta payloads or decode failures are returned as `Some(payload.to_vec())` (forward unchanged).
pub fn filter_payload_for_viewer(
    payload: &[u8],
    viewer_pos: Option<Vec3>,
    viewer_user: Option<UserId>,
    grid: &orbisync_interest::UniformGrid,
    views: &[orbisync_world_runtime::actor::EntityInterestView],
    subscribed: &mut std::collections::HashMap<EntityId, bool>,
) -> Option<Vec<u8>> {
    filter_payload_for_viewer_with_roles(
        payload,
        viewer_pos,
        viewer_user,
        grid,
        views,
        None,
        subscribed,
    )
}

/// Role-aware payload filter used by authenticated connections.
pub fn filter_payload_for_viewer_with_roles(
    payload: &[u8],
    viewer_pos: Option<Vec3>,
    viewer_user: Option<UserId>,
    grid: &orbisync_interest::UniformGrid,
    views: &[orbisync_world_runtime::actor::EntityInterestView],
    viewer_roles: Option<&std::collections::BTreeSet<orbisync_domain::RoleId>>,
    subscribed: &mut std::collections::HashMap<EntityId, bool>,
) -> Option<Vec<u8>> {
    if viewer_pos.is_none() {
        return Some(payload.to_vec());
    }
    let views = views.iter().cloned().collect();
    filter_payload_for_viewer_indexed(payload, viewer_pos, viewer_user, grid, &views, viewer_roles, subscribed)
}

/// Filters using the actor's shared index, without per-receiver world scans or
/// lookup/set construction. Slice-based helpers remain compatibility adapters.
pub fn filter_payload_for_viewer_indexed(
    payload: &[u8],
    viewer_pos: Option<Vec3>,
    viewer_user: Option<UserId>,
    grid: &orbisync_interest::UniformGrid,
    views: &orbisync_world_runtime::InterestSnapshot,
    viewer_roles: Option<&std::collections::BTreeSet<orbisync_domain::RoleId>>,
    subscribed: &mut std::collections::HashMap<EntityId, bool>,
) -> Option<Vec<u8>> {
    if viewer_pos.is_none() {
        return Some(payload.to_vec());
    }
    let result = filter_payload_with_index(payload, viewer_pos, viewer_user, grid, views, viewer_roles, subscribed);
    subscribed.retain(|id, _| views.contains(*id));
    result
}

// Production sinks prune only when entity membership changes. All per-entity
// updates below still use current positions, owners and policies every time.
fn filter_payload_with_index(
    payload: &[u8],
    viewer_pos: Option<Vec3>,
    viewer_user: Option<UserId>,
    grid: &orbisync_interest::UniformGrid,
    views: &orbisync_world_runtime::InterestSnapshot,
    viewer_roles: Option<&std::collections::BTreeSet<orbisync_domain::RoleId>>,
    subscribed: &mut std::collections::HashMap<EntityId, bool>,
) -> Option<Vec<u8>> {
    let Some(viewer) = viewer_pos else {
        return Some(payload.to_vec());
    };
    let mut envelope = match decode_envelope(payload) {
        Ok(e) => e,
        Err(_) => return Some(payload.to_vec()),
    };
    let payload_taken = envelope.payload.take();
    match payload_taken {
        Some(envelope::Payload::StateDelta(delta)) => {
            let mut kept = Vec::new();
            // Batch hysteresis updates to avoid simultaneous &mut and & borrow of `subscribed` (W-6 borrow issue).
            // No clone of `subscribed`; collect updates while holding only &*subscribed, then write back.
            let mut updates: Vec<(EntityId, bool)> = Vec::new();
            for es in delta.entities {
                // Try to resolve the authoritative view for policy-aware filtering.
                if let Ok(eid) = es.entity_id.parse::<EntityId>() {
                    if let Some(view) = views.get(eid) {
                        let visible = crate::interest_filter::filter_visible_views_with_roles(
                            Some(viewer),
                            std::slice::from_ref(view),
                            grid,
                            viewer_user,
                            viewer_roles,
                            Some(&*subscribed),
                        );
                        let is_visible = !visible.is_empty();
                        updates.push((eid, is_visible));
                        if is_visible {
                            kept.push(es);
                        }
                        continue;
                    }
                    // Fallback when entity not in snapshot but we have an EntityId: distance + cell check (no policy).
                    // Use hysteresis from `subscribed` so 31 m still retains after 29 m subscription (N-3).
                    let currently_subscribed = subscribed.get(&eid).copied().unwrap_or(false);
                    match es.transform.as_ref() {
                        None => {
                            updates.push((eid, true));
                            kept.push(es);
                        }
                        Some(t) => {
                            let entity_pos =
                                match Vec3::new(t.position_x, t.position_y, t.position_z) {
                                    Ok(v) => v,
                                    Err(_) => {
                                        updates.push((eid, false));
                                        continue;
                                    }
                                };
                            let dist = orbisync_interest::UniformGrid::euclidean_distance(
                                viewer, entity_pos,
                            );
                            if !dist.is_finite() {
                                updates.push((eid, false));
                                continue;
                            }
                            if !grid.should_retain(dist, currently_subscribed) {
                                updates.push((eid, false));
                                continue;
                            }
                            if !grid.is_in_subscribed_area(viewer, entity_pos) {
                                updates.push((eid, false));
                                continue;
                            }
                            updates.push((eid, true));
                            kept.push(es);
                        }
                    }
                    continue;
                }
                // Fallback when entity_id is not parseable: no hysteresis key, use non-subscribed check.
                match es.transform.as_ref() {
                    None => kept.push(es),
                    Some(t) => {
                        let entity_pos = match Vec3::new(t.position_x, t.position_y, t.position_z) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        let dist =
                            orbisync_interest::UniformGrid::euclidean_distance(viewer, entity_pos);
                        if !dist.is_finite() {
                            continue;
                        }
                        if !grid.should_retain(dist, false) {
                            continue;
                        }
                        if !grid.is_in_subscribed_area(viewer, entity_pos) {
                            continue;
                        }
                        // No policy -> assume Spatial, so distance already checked.
                        kept.push(es);
                    }
                }
            }
            // Write back hysteresis state after the immutable borrow ends (W-6).
            for (id, is_visible) in updates {
                if views.contains(id) {
                    subscribed.insert(id, is_visible);
                } else {
                    subscribed.remove(&id);
                }
            }
            if kept.is_empty() {
                return None;
            }
            let filtered_delta = orbisync_protocol::v1::StateDelta {
                from_revision: delta.from_revision,
                to_revision: delta.to_revision,
                entities: kept,
            };
            envelope.payload = Some(envelope::Payload::StateDelta(filtered_delta));
            let mut buf = Vec::new();
            if envelope.encode(&mut buf).is_ok() {
                Some(buf)
            } else {
                Some(payload.to_vec())
            }
        }
        Some(envelope::Payload::EntityCommand(cmd)) => {
            // D-15: EntityCommand is reliable, not latest-wins. Filter via interest before delivery.
            // For spawn/update the authoritative view is in `views`; for delete the view is gone
            // so we use the tombstone visibility/position/owner synthesized into `cmd.arguments`.
            let Ok(eid) = cmd.entity_id.parse::<EntityId>() else {
                return Some(payload.to_vec());
            };
            let op = cmd.operation.to_lowercase();
            let transfer_participant = op == "transfer_ownership"
                && cmd.arguments.as_ref().is_some_and(|args| {
                    ["previous_owner_id", "new_owner_id"].iter().any(|key| {
                        extract_string_field(args, key)
                            .and_then(|value| value.parse::<UserId>().ok())
                            == viewer_user
                    })
                });
            let is_visible = if transfer_participant {
                true
            } else if let Some(view) = views.get(eid) {
                let visible = crate::interest_filter::filter_visible_views_with_roles(
                    Some(viewer),
                    std::slice::from_ref(view),
                    grid,
                    viewer_user,
                    viewer_roles,
                    Some(&*subscribed),
                );
                !visible.is_empty()
            } else if op == "delete" || op == "spawn" {
                if let Some(args) = &cmd.arguments {
                    let visibility = match extract_string_field(args, "visibility") {
                        Some(s) => match s.to_lowercase().as_str() {
                            "global" => VisibilityPolicy::Global,
                            "owner_only" | "owneronly" | "owner-only" => {
                                VisibilityPolicy::OwnerOnly
                            }
                            "spatial" => {
                                let r = extract_number_field(args, "visibility_radius")
                                    .or_else(|| extract_number_field(args, "radius"))
                                    .unwrap_or(30.0);
                                VisibilityPolicy::spatial(r as f32)
                                    .unwrap_or(VisibilityPolicy::Global)
                            }
                            _ => VisibilityPolicy::Global,
                        },
                        None => VisibilityPolicy::Global,
                    };
                    let position = {
                        let x = extract_number_field(args, "position_x");
                        let y = extract_number_field(args, "position_y");
                        let z = extract_number_field(args, "position_z");
                        match (x, y, z) {
                            (Some(px), Some(py), Some(pz)) => Vec3::new(px, py, pz).ok(),
                            _ => None,
                        }
                    };
                    let owner =
                        extract_string_field(args, "owner").and_then(|s| s.parse::<UserId>().ok());
                    let is_owner = match (viewer_user, owner) {
                        (Some(v), Some(o)) => v == o,
                        _ => false,
                    };
                    let viewer_context = orbisync_interest::ViewerContext {
                        user: viewer_user,
                        roles: viewer_roles,
                        is_owner,
                    };
                    let currently_subscribed = subscribed.get(&eid).copied().unwrap_or(false);
                    if let Some(pos) = position {
                        grid.is_entity_visible(
                            viewer,
                            pos,
                            &visibility,
                            viewer_context,
                            currently_subscribed,
                        )
                    } else {
                        grid.is_visible_without_position(&visibility, viewer_context)
                    }
                } else {
                    false
                }
            } else {
                false
            };
            // Update hysteresis for this entity (single-entity batch)
            if op == "delete" || !views.contains(eid) {
                subscribed.remove(&eid);
            } else {
                subscribed.insert(eid, is_visible);
            }
            if is_visible {
                Some(payload.to_vec())
            } else {
                None
            }
        }
        _ => Some(payload.to_vec()),
    }
}

/// Returns `true` if the viewer should follow `entity_id` (N-4).
///
/// The live path pins `viewer_pos` to the first entity the connection moves,
/// so moving a second entity does not shift the viewer's interest center.
/// `viewer_entity` holds the already-pinned id, if any. This pure helper
/// exists so the pinning rule can be unit tested without a live WebSocket.
#[must_use]
pub fn should_follow_viewer(viewer_entity: Option<EntityId>, entity_id: EntityId) -> bool {
    viewer_entity.is_none_or(|id| id == entity_id)
}

/// Sends an `ErrorMessage` envelope on `socket` (N-2: deduplicates 5 call sites).
async fn send_error(
    socket: &mut RealtimeSocket,
    code: &str,
    message: String,
    request_message_id: String,
    negotiated_minor: u32,
    sequence: u64,
    clock: &dyn Clock,
    instance_id: &str,
) {
    send_error_with_retryable(
        socket,
        code,
        message,
        request_message_id,
        negotiated_minor,
        sequence,
        clock,
        instance_id,
        false,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn send_outbound_size_error(
    socket: &mut RealtimeSocket,
    violation: MessageSizeViolation,
    request_message_id: String,
    negotiated_minor: u32,
    sequence: u64,
    clock: &dyn Clock,
    instance_id: &str,
) {
    send_error(
        socket,
        "MESSAGE_TOO_LARGE",
        format!(
            "outbound message size {} exceeds limit {}",
            violation.got_bytes, violation.max_bytes
        ),
        request_message_id,
        negotiated_minor,
        sequence,
        clock,
        instance_id,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn send_error_with_retryable(
    socket: &mut RealtimeSocket,
    code: &str,
    message: String,
    request_message_id: String,
    negotiated_minor: u32,
    sequence: u64,
    clock: &dyn Clock,
    instance_id: &str,
    retryable: bool,
) {
    send_error_with_message_id(
        socket,
        code,
        message,
        request_message_id,
        uuid::Uuid::now_v7().to_string(),
        negotiated_minor,
        sequence,
        clock,
        instance_id,
        retryable,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn send_error_with_message_id(
    socket: &mut RealtimeSocket,
    code: &str,
    message: String,
    request_message_id: String,
    message_id: String,
    negotiated_minor: u32,
    sequence: u64,
    clock: &dyn Clock,
    instance_id: &str,
    retryable: bool,
) {
    let err = orbisync_protocol::v1::ErrorMessage {
        code: code.to_owned(),
        message,
        request_message_id,
        retryable,
    };
    let env = Envelope {
        protocol_major: orbisync_protocol::PROTOCOL_MAJOR,
        protocol_minor: negotiated_minor,
        message_id,
        sequence,
        sent_at_unix_ms: clock.now().to_unix_millis().unwrap_or(0),
        instance_id: instance_id.to_owned(),
        payload: Some(envelope::Payload::Error(err)),
    };
    let mut buf = Vec::new();
    if env.encode(&mut buf).is_ok() {
        // Reason: best-effort send on potentially closed socket; failure means peer already disconnected and is not actionable (A-3 allow with reason).
        #[allow(clippy::let_underscore_must_use)]
        let _ = socket.write(Message::Binary(buf.into())).await;
    }
}
