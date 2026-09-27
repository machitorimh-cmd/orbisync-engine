# Recover an existing administrator password

Use `reset-admin-password` when all administrator sessions and passwords are lost.
It requires local installation access and the installation's database operator
credentials. It does not start a server or expose a remote recovery endpoint.
Use the updated executable containing this command; older builds cannot recover
an existing administrator with `bootstrap-admin`.

From the installation root in PowerShell, with Node 24 available and the existing
`.env` containing `DATABASE_URL`, run:

```powershell
node --env-file=.env -e "const r=require('node:child_process').spawnSync('./bin/web-admin/orbisync-server.exe',['reset-admin-password','--login-id','admin','--password-denylist',process.env.ORBISYNC_PASSWORD_DENYLIST_FILE||'deploy/dev-password-denylist.txt','--password-output','.admin-recovery/temporary-password.txt'],{stdio:'inherit'});if(r.error){console.error(r.error.message)}process.exit(r.status??1)"
```

Replace `admin` only if the existing administrator uses a different login. The
command creates `.admin-recovery` privately if absent. The output file must not
already exist. An existing output directory must be private (mode 0700 on Unix;
on Windows, owned by the current user with access limited to that user, SYSTEM,
and Administrators). It refuses a symlink/reparse-point output directory.

This command uses `ORBISYNC_PASSWORD_DENYLIST_FILE` when configured, otherwise
the existing development corpus at `deploy/dev-password-denylist.txt`. It does
not generate or replace a corpus. For another installation, set that variable to
its approved 10,000-entry corpus. Existing password policy and Argon2
settings apply. If the installation uses an explicit TOML configuration, put
`'--config','path/to/config.toml'` before `'reset-admin-password'`; the configured
database URL environment variable and normal config discovery are respected.
The executable itself does not load `.env`; Node loads it for this invocation.

Open `.admin-recovery/temporary-password.txt` privately. In the existing web-admin
login page, sign in with the same login and that temporary password, then complete
the required password change. Sign in again with the new password if prompted.
Remove the temporary-password file after changing the password. Never put its
contents in chat, logs, or source control.

The target must be an existing active local account holding
`admin.users.credentials.reset`; missing, nonadministrator, and inactive targets
are refused. Recovery clears credential lockout, requires a password change,
and atomically revokes existing sessions (including access and refresh tokens).
It records `administrator.password_recovered` with no authenticated actor and the
target user ID. It does not create users, grant roles, activate disabled users,
run migrations, or modify world/game records.

If recovery fails before commit, no password change is made; an empty reserved
output file may remain. Use a fresh output filename for another attempt. If disk
output fails after the reset commits, the error explicitly says so: run recovery
again with a new private output file to obtain a usable temporary credential.
Database commit acknowledgement can also be uncertain after a connection loss;
another explicit recovery to a fresh file supersedes the uncertain credential.
The database transaction and filesystem output cannot commit atomically.
