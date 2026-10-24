INSERT INTO permissions(name) VALUES
    ('admin.sessions.read'), ('admin.sessions.revoke'), ('admin.auth.keys.reload')
ON CONFLICT DO NOTHING;

-- No historical builtin-role marker exists. Match the original bootstrap
-- signature and its entire original permission set. Do not elevate a custom,
-- renamed or deliberately restricted role based on its display name alone.
-- These existing capabilities already permit creating and assigning roles.
WITH eligible AS (
    SELECT r.id FROM roles r
    WHERE r.name = 'Administrator'
      AND r.description = 'Initial administration role'
      AND NOT EXISTS (
          SELECT required.permission FROM (VALUES
              ('admin.users.read'), ('admin.users.create'), ('admin.users.update'),
              ('admin.users.status'), ('admin.users.import'), ('admin.users.credentials.reset'),
              ('admin.roles.read'), ('admin.roles.create'), ('admin.roles.update'),
              ('admin.roles.delete'), ('admin.roles.assign'), ('admin.audit.read'),
              ('admin.diagnostics.read'), ('admin.worlds.create'), ('admin.worlds.read'),
              ('admin.worlds.update'), ('admin.worlds.archive')
          ) AS required(permission)
          WHERE NOT EXISTS (SELECT 1 FROM role_permissions rp
                            WHERE rp.role_id=r.id AND rp.permission_name=required.permission)
      )
), granted AS (
    INSERT INTO role_permissions(role_id,permission_name)
    SELECT e.id,p.permission FROM eligible e CROSS JOIN (VALUES
        ('admin.sessions.read'), ('admin.sessions.revoke'), ('admin.auth.keys.reload')
    ) AS p(permission)
    ON CONFLICT DO NOTHING
    RETURNING role_id
)
UPDATE roles SET revision=revision+1 WHERE id IN (SELECT role_id FROM granted);
