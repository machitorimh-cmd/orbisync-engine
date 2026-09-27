//! Entity aggregate and related value objects.

use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
};

use crate::{
    DomainError, DomainErrorKind, EntityId, InstanceId, Revision, Timestamp, UserId,
    transform::{Transform, Vec3},
};

/// Ephemeral animation state for an avatar.
#[derive(Debug, Clone, PartialEq)]
pub struct Animation {
    clip: String,
    time: f32,
    speed: f32,
    looping: bool,
}

impl Animation {
    /// Creates animation state, applying the component-key character rules.
    pub fn new(
        clip: impl Into<String>,
        time: f32,
        speed: f32,
        looping: bool,
    ) -> Result<Self, DomainError> {
        let clip = clip.into();
        if !(1..=64).contains(&clip.len())
            || !clip.bytes().all(|b| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'_')
            })
        {
            return Err(invalid(
                "animation clip must be 1..=64 ASCII bytes using lowercase letters, digits, dots, hyphens, or underscores",
            ));
        }
        if !time.is_finite() || time < 0.0 {
            return Err(invalid("animation time must be finite and non-negative"));
        }
        if !speed.is_finite() {
            return Err(invalid("animation speed must be finite"));
        }
        Ok(Self {
            clip,
            time,
            speed,
            looping,
        })
    }

    /// Returns the animation clip name.
    #[must_use]
    pub fn clip(&self) -> &str {
        &self.clip
    }
    /// Returns playback position in seconds.
    #[must_use]
    pub const fn time(&self) -> f32 {
        self.time
    }
    /// Returns playback speed.
    #[must_use]
    pub const fn speed(&self) -> f32 {
        self.speed
    }
    /// Returns whether playback loops.
    #[must_use]
    pub const fn looping(&self) -> bool {
        self.looping
    }
}

/// Presence state advertised to other participants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PresenceState {
    /// Participant is available.
    Online,
    /// Participant is temporarily away.
    Away,
    /// Participant is busy.
    Busy,
}

impl PresenceState {
    /// Returns the canonical wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Online => "online",
            Self::Away => "away",
            Self::Busy => "busy",
        }
    }

    /// Parses the canonical wire spelling.
    pub fn parse(value: &str) -> Result<Self, DomainError> {
        match value {
            "online" => Ok(Self::Online),
            "away" => Ok(Self::Away),
            "busy" => Ok(Self::Busy),
            _ => Err(invalid("presence state must be online/away/busy")),
        }
    }
}

/// Ephemeral presence payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Presence {
    state: PresenceState,
    last_seen: i64,
}

impl Presence {
    /// Creates presence state. Unix milliseconds are represented directly as i64.
    #[must_use]
    pub const fn new(state: PresenceState, last_seen: i64) -> Self {
        Self { state, last_seen }
    }
    /// Returns the presence state.
    #[must_use]
    pub const fn state(self) -> PresenceState {
        self.state
    }
    /// Returns the last-seen Unix timestamp in milliseconds.
    #[must_use]
    pub const fn last_seen(self) -> i64 {
        self.last_seen
    }
}

fn invalid(detail: impl Into<String>) -> DomainError {
    DomainError::new(DomainErrorKind::InvalidValue, detail)
}

/// Server-owned core component names.
///
/// These names are reserved here so the domain and runtime share one
/// canonical vocabulary. `core.velocity` has a typed [`Vec3`] value below;
/// `core.animation` and `core.presence` carry the minimal transient state needed
/// to show avatar actions and availability. This schema is an orchestrator
/// decision because the design specification left these payloads undefined;
/// revisit it when the specification is finalized. Animation deliberately has
/// no blending/layers, and presence deliberately has no gaze or speech state.
pub const CORE_TRANSFORM: &str = "core.transform";
/// Reserved server-owned velocity component name.
pub const CORE_VELOCITY: &str = "core.velocity";
/// Reserved server-owned animation component name.
pub const CORE_ANIMATION: &str = "core.animation";
/// Reserved server-owned presence component name.
pub const CORE_PRESENCE: &str = "core.presence";

/// Kind of an entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntityKind {
    /// User avatar.
    Avatar,
    /// Generic object.
    Object,
    /// Trigger volume.
    Trigger,
}

impl EntityKind {
    /// Returns canonical string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Avatar => "avatar",
            Self::Object => "object",
            Self::Trigger => "trigger",
        }
    }

    /// Parses canonical string.
    ///
    /// # Errors
    ///
    /// Returns invalid-value for unknown.
    pub fn parse(value: &str) -> Result<Self, DomainError> {
        match value {
            "avatar" => Ok(Self::Avatar),
            "object" => Ok(Self::Object),
            "trigger" => Ok(Self::Trigger),
            _ => Err(invalid("entity kind must be avatar/object/trigger")),
        }
    }
}

/// Visibility policy (spec §17.4).
#[derive(Debug, Clone, PartialEq)]
pub enum VisibilityPolicy {
    /// Visible to all members.
    Global,
    /// Visible within a spatial radius.
    Spatial {
        /// Radius in meters.
        radius: f32,
    },
    /// Visible only to the owner.
    OwnerOnly,
    /// Visible to viewers having at least one of the listed roles.
    RoleRestricted {
        /// Role identifiers accepted by this policy.
        roles: Arc<BTreeSet<crate::RoleId>>,
    },
    /// Visible only to the listed users.
    Explicit {
        /// User identifiers accepted by this policy.
        users: Arc<BTreeSet<UserId>>,
    },
    /// Application-defined policy. Unknown tags fail closed in the interest evaluator.
    Custom {
        /// Application-defined policy tag.
        tag: CustomVisibilityTag,
    },
}

/// Validated application-defined visibility policy tag.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CustomVisibilityTag(Arc<str>);

impl CustomVisibilityTag {
    /// Creates a tag using the component-key character set.
    pub fn new(value: impl AsRef<str>) -> Result<Self, DomainError> {
        let value = value.as_ref();
        if !(1..=64).contains(&value.len())
            || !value.bytes().all(|b| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'_')
            })
        {
            return Err(invalid(
                "visibility tag must be 1..=64 ASCII bytes using lowercase letters, digits, dots, hyphens, or underscores",
            ));
        }
        Ok(Self(Arc::from(value)))
    }

    /// Returns the tag text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl VisibilityPolicy {
    /// Creates a role-restricted policy. Empty sets are rejected.
    pub fn role_restricted<I: IntoIterator<Item = crate::RoleId>>(
        roles: I,
    ) -> Result<Self, DomainError> {
        let roles: BTreeSet<_> = roles.into_iter().collect();
        Self::from_role_set(roles)
    }

    /// Validates an already assembled role set without rebuilding or sorting it.
    /// Streaming callers can assemble the set with their own cooperative budget.
    pub fn from_role_set(roles: BTreeSet<crate::RoleId>) -> Result<Self, DomainError> {
        if roles.is_empty() || roles.len() > 16 {
            return Err(invalid("visibility roles must contain 1..=16 entries"));
        }
        Ok(Self::RoleRestricted {
            roles: Arc::new(roles),
        })
    }

    /// Creates an explicit-user policy. Empty sets are rejected.
    pub fn explicit<I: IntoIterator<Item = UserId>>(users: I) -> Result<Self, DomainError> {
        let users: BTreeSet<_> = users.into_iter().collect();
        Self::from_user_set(users)
    }

    /// Validates an already assembled user set without rebuilding or sorting it.
    /// Uses the same cardinality rule as `explicit`.
    pub fn from_user_set(users: BTreeSet<UserId>) -> Result<Self, DomainError> {
        if users.is_empty() || users.len() > 64 {
            return Err(invalid("visibility users must contain 1..=64 entries"));
        }
        Ok(Self::Explicit {
            users: Arc::new(users),
        })
    }

    /// Creates a custom policy with a validated tag.
    pub fn custom(tag: impl AsRef<str>) -> Result<Self, DomainError> {
        Ok(Self::Custom {
            tag: CustomVisibilityTag::new(tag)?,
        })
    }

    /// Validates radius for spatial.
    ///
    /// # Errors
    ///
    /// Returns invalid-value when radius is non-finite or non-positive.
    pub fn spatial(radius: f32) -> Result<Self, DomainError> {
        if !radius.is_finite() || radius <= 0.0 || radius > 10_000.0 {
            return Err(invalid("spatial radius must be finite 0..10000"));
        }
        Ok(Self::Spatial { radius })
    }
}

/// Entity aggregate root (`domain-model.md` §3.5).
#[derive(Debug, Clone, PartialEq)]
pub struct Entity {
    id: EntityId,
    instance_id: InstanceId,
    kind: EntityKind,
    owner: Option<UserId>,
    transform: Option<Transform>,
    /// Current velocity in world units per second. This is ephemeral state and
    /// is intentionally not included in persistence checkpoints.
    velocity: Option<Vec3>,
    /// Current animation, intentionally excluded from persistence checkpoints.
    animation: Option<Animation>,
    /// Current presence, intentionally excluded from persistence checkpoints.
    presence: Option<Presence>,
    visibility: VisibilityPolicy,
    revision: Revision,
    created_at: Timestamp,
    updated_at: Timestamp,
    components: HashMap<String, Vec<u8>>,
}

impl Entity {
    /// Creates an entity.
    pub fn new(
        id: EntityId,
        instance_id: InstanceId,
        kind: EntityKind,
        owner: Option<UserId>,
        transform: Option<Transform>,
        visibility: VisibilityPolicy,
        now: Timestamp,
    ) -> Self {
        Self {
            id,
            instance_id,
            kind,
            owner,
            transform,
            velocity: None,
            animation: None,
            presence: None,
            visibility,
            revision: Revision::from_u64(1),
            created_at: now,
            updated_at: now,
            components: HashMap::new(),
        }
    }

    /// Restores an entity from durable storage without applying a mutation.
    ///
    /// Ephemeral velocity, animation, and presence state are intentionally
    /// reset. All durable fields are validated before the entity is admitted
    /// into runtime state.
    ///
    /// # Errors
    ///
    /// Returns an invalid-value error when the persisted revision, timestamps,
    /// or custom components violate the entity invariants.
    #[allow(clippy::too_many_arguments)]
    pub fn from_persisted(
        id: EntityId,
        instance_id: InstanceId,
        kind: EntityKind,
        owner: Option<UserId>,
        transform: Option<Transform>,
        visibility: VisibilityPolicy,
        revision: Revision,
        created_at: Timestamp,
        updated_at: Timestamp,
        components: HashMap<String, Vec<u8>>,
    ) -> Result<Self, DomainError> {
        if revision.as_u64() == 0 {
            return Err(invalid("entity revision must be >= 1"));
        }
        if updated_at < created_at {
            return Err(invalid("entity updated_at must not precede created_at"));
        }
        if components.len() > 16 {
            return Err(invalid("too many components"));
        }
        for (key, payload) in &components {
            validate_custom_component(key, payload)?;
        }
        Ok(Self {
            id,
            instance_id,
            kind,
            owner,
            transform,
            velocity: None,
            animation: None,
            presence: None,
            visibility,
            revision,
            created_at,
            updated_at,
            components,
        })
    }

    /// Returns id.
    #[must_use]
    pub const fn id(&self) -> EntityId {
        self.id
    }

    /// Returns instance_id.
    #[must_use]
    pub const fn instance_id(&self) -> InstanceId {
        self.instance_id
    }

    /// Returns kind.
    #[must_use]
    pub const fn kind(&self) -> EntityKind {
        self.kind
    }

    /// Returns owner.
    #[must_use]
    pub const fn owner(&self) -> Option<UserId> {
        self.owner
    }

    /// Returns transform.
    #[must_use]
    pub const fn transform(&self) -> Option<Transform> {
        self.transform
    }

    /// Returns the current ephemeral velocity, if present.
    #[must_use]
    pub const fn velocity(&self) -> Option<Vec3> {
        self.velocity
    }

    /// Returns the current ephemeral animation, if present.
    #[must_use]
    pub fn animation(&self) -> Option<&Animation> {
        self.animation.as_ref()
    }

    /// Returns the current ephemeral presence, if present.
    #[must_use]
    pub const fn presence(&self) -> Option<Presence> {
        self.presence
    }

    /// Updates ephemeral animation state, enforcing entity revision.
    pub fn update_animation(
        &mut self,
        expected_revision: Revision,
        animation: Animation,
        now: Timestamp,
    ) -> Result<(), DomainError> {
        self.revision.ensure_matches(expected_revision)?;
        self.animation = Some(animation);
        self.revision = self.revision.next()?;
        self.updated_at = now;
        Ok(())
    }

    /// Updates ephemeral presence state, enforcing entity revision.
    pub fn update_presence(
        &mut self,
        expected_revision: Revision,
        presence: Presence,
        now: Timestamp,
    ) -> Result<(), DomainError> {
        self.revision.ensure_matches(expected_revision)?;
        self.presence = Some(presence);
        self.revision = self.revision.next()?;
        self.updated_at = now;
        Ok(())
    }

    /// Returns visibility.
    #[must_use]
    pub const fn visibility(&self) -> &VisibilityPolicy {
        &self.visibility
    }

    /// Returns revision.
    #[must_use]
    pub const fn revision(&self) -> Revision {
        self.revision
    }

    /// Returns created_at.
    #[must_use]
    pub const fn created_at(&self) -> Timestamp {
        self.created_at
    }

    /// Returns updated_at.
    #[must_use]
    pub const fn updated_at(&self) -> Timestamp {
        self.updated_at
    }

    /// Updates the transform, enforcing revision.
    ///
    /// # Errors
    ///
    /// Returns revision-mismatch or overflow.
    pub fn update_transform(
        &mut self,
        expected_revision: Revision,
        transform: Transform,
        now: Timestamp,
    ) -> Result<(), DomainError> {
        self.revision.ensure_matches(expected_revision)?;
        self.transform = Some(transform);
        self.revision = self.revision.next()?;
        self.updated_at = now;
        Ok(())
    }

    /// Updates the ephemeral velocity, enforcing entity revision.
    ///
    /// Velocity is a core standard component (`core.velocity`) represented by
    /// a [`Vec3`]. It is not persisted by checkpoint serialization.
    ///
    /// # Errors
    ///
    /// Returns revision-mismatch or overflow.
    pub fn update_velocity(
        &mut self,
        expected_revision: Revision,
        velocity: Vec3,
        now: Timestamp,
    ) -> Result<(), DomainError> {
        self.revision.ensure_matches(expected_revision)?;
        self.velocity = Some(velocity);
        self.revision = self.revision.next()?;
        self.updated_at = now;
        Ok(())
    }

    /// Transfers ownership. Server-authoritative; caller must have validated
    /// permission.
    ///
    /// # Errors
    ///
    /// Returns revision-mismatch or overflow.
    pub fn transfer_owner(
        &mut self,
        expected_revision: Revision,
        new_owner: Option<UserId>,
        now: Timestamp,
    ) -> Result<(), DomainError> {
        self.revision.ensure_matches(expected_revision)?;
        self.owner = new_owner;
        self.revision = self.revision.next()?;
        self.updated_at = now;
        Ok(())
    }

    /// Returns a component payload if present.
    #[must_use]
    pub fn component(&self, key: &str) -> Option<&[u8]> {
        self.components.get(key).map(Vec::as_slice)
    }

    /// Returns all components.
    #[must_use]
    pub fn components(&self) -> &HashMap<String, Vec<u8>> {
        &self.components
    }

    /// Updates or inserts a component, enforcing revision and limits.
    ///
    /// Limits (M4): namespaced key 1..128 bytes, payload <= 4096 bytes, at
    /// most 16 components per entity (`domain-model.md` DM-04).
    ///
    /// # Errors
    ///
    /// Returns invalid-value, revision-mismatch or overflow.
    pub fn update_component(
        &mut self,
        expected_revision: Revision,
        key: String,
        payload: Vec<u8>,
        now: Timestamp,
    ) -> Result<(), DomainError> {
        self.revision.ensure_matches(expected_revision)?;
        validate_custom_component(&key, &payload)?;
        if !self.components.contains_key(&key) && self.components.len() >= 16 {
            return Err(invalid("too many components"));
        }
        self.components.insert(key, payload);
        self.revision = self.revision.next()?;
        self.updated_at = now;
        Ok(())
    }
}

fn validate_custom_component(key: &str, payload: &[u8]) -> Result<(), DomainError> {
    if key.is_empty() || key.len() > 128 {
        return Err(invalid("component_key must be 1..128 bytes"));
    }
    let Some((namespace, _name)) = key.split_once('.') else {
        return Err(invalid("component_key must include a namespace"));
    };
    if namespace.is_empty() || key.ends_with('.') {
        return Err(invalid(
            "component_key namespace and name must not be empty",
        ));
    }
    if !key.bytes().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
    }) {
        return Err(invalid(
            "component_key may contain only lowercase letters, digits, dots, hyphens, and underscores",
        ));
    }
    if namespace == "core" {
        return Err(invalid("core component namespace is server-owned"));
    }
    if payload.len() > 4096 {
        return Err(invalid("payload exceeds 4096 bytes"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Animation, Entity, EntityKind, Presence, PresenceState, VisibilityPolicy};
    use crate::{EntityId, InstanceId, Revision, Timestamp, transform::Transform};

    fn now() -> Timestamp {
        Timestamp::from_unix_millis(1_000).expect("valid")
    }

    #[test]
    fn create_and_update_transform() {
        let mut entity = Entity::new(
            EntityId::generate(),
            InstanceId::generate(),
            EntityKind::Object,
            None,
            None,
            VisibilityPolicy::Global,
            now(),
        );
        let rev = entity.revision();
        entity
            .update_transform(rev, Transform::identity(), now())
            .expect("update");
        assert_eq!(entity.revision(), Revision::from_u64(2));
        assert!(entity.transform().is_some());
    }

    #[test]
    fn create_and_update_velocity() {
        let mut entity = Entity::new(
            EntityId::generate(),
            InstanceId::generate(),
            EntityKind::Object,
            None,
            None,
            VisibilityPolicy::Global,
            now(),
        );
        let velocity = crate::Vec3::new(1.0_f32, 2.0_f32, 3.0_f32).expect("valid velocity");
        let revision = entity.revision();
        entity
            .update_velocity(revision, velocity, now())
            .expect("velocity update");
        assert_eq!(entity.velocity(), Some(velocity));
        assert_eq!(entity.revision(), Revision::from_u64(2));
    }

    #[test]
    fn animation_validates_schema_and_updates_ephemeral_state() {
        assert!(Animation::new("A", 0.0, 1.0, false).is_err());
        assert!(Animation::new("a".repeat(65), 0.0, 1.0, false).is_err());
        assert!(Animation::new("walk", -0.1, 1.0, true).is_err());
        assert!(Animation::new("walk", 0.0, f32::INFINITY, true).is_err());
        let animation = Animation::new("walk", 1.5, -1.0, true).expect("valid animation");
        let mut entity = entity();
        let revision = entity.revision();
        entity
            .update_animation(revision, animation.clone(), now())
            .expect("animation update");
        assert_eq!(entity.animation(), Some(&animation));
    }

    #[test]
    fn presence_accepts_only_canonical_states_and_is_ephemeral() {
        assert_eq!(
            PresenceState::parse("online").expect("valid"),
            PresenceState::Online
        );
        assert!(PresenceState::parse("offline").is_err());
        let mut entity = entity();
        let presence = Presence::new(PresenceState::Away, 1_000);
        entity
            .update_presence(entity.revision(), presence, now())
            .expect("presence update");
        assert_eq!(entity.presence(), Some(presence));
    }

    #[test]
    fn rejects_revision_mismatch() {
        let mut entity = Entity::new(
            EntityId::generate(),
            InstanceId::generate(),
            EntityKind::Object,
            None,
            None,
            VisibilityPolicy::Global,
            now(),
        );
        let wrong = Revision::from_u64(99);
        assert!(
            entity
                .update_transform(wrong, Transform::identity(), now())
                .is_err()
        );
    }

    fn entity() -> Entity {
        Entity::new(
            EntityId::generate(),
            InstanceId::generate(),
            EntityKind::Object,
            None,
            None,
            VisibilityPolicy::Global,
            now(),
        )
    }

    #[test]
    fn component_keys_require_namespaces_and_lowercase_custom_names() {
        let mut entity = entity();
        let revision = entity.revision();
        for key in ["door", "Core.foo", "core.foo", "com.example."] {
            assert!(
                entity
                    .update_component(revision, key.to_owned(), vec![1], now())
                    .is_err(),
                "key should be rejected: {key}"
            );
        }
        entity
            .update_component(revision, "com.example.door".to_owned(), vec![1], now())
            .expect("custom namespaced component should be accepted");
    }

    #[test]
    fn component_size_and_count_limits_remain_enforced() {
        let mut entity = entity();
        let revision = entity.revision();
        assert!(
            entity
                .update_component(
                    revision,
                    "com.example.large".to_owned(),
                    vec![0; 4097],
                    now()
                )
                .is_err()
        );

        let mut revision = revision;
        for index in 0..16 {
            entity
                .update_component(
                    revision,
                    format!("com.example.component_{index}"),
                    vec![index as u8],
                    now(),
                )
                .expect("component within count limit");
            revision = entity.revision();
        }
        assert!(
            entity
                .update_component(
                    revision,
                    "com.example.seventeenth".to_owned(),
                    vec![1],
                    now()
                )
                .is_err()
        );
    }
}
