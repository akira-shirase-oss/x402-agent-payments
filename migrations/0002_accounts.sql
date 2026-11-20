-- 0002: the accounts kit (crates/xpay-accounts, docs/ACCOUNTS.md). A product that does
-- not take the kit drops this file, the kit's crate and the kit's rows in the manifest.
--
-- Secrets are never stored: invite codes, session ids, email-link tokens and API tokens
-- are kept as SHA-256 digests (32 bytes); passwords as Argon2id PHC strings. Emails are
-- stored normalized (trimmed, lower-case).

CREATE TABLE users (
    id uuid PRIMARY KEY,
    email text NOT NULL UNIQUE CHECK (email = lower(email) AND length(email) BETWEEN 3 AND 254),
    password_hash text NOT NULL CHECK (length(password_hash) BETWEEN 1 AND 255),
    verified_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE invites (
    digest bytea PRIMARY KEY CHECK (length(digest) = 32),
    created_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL,
    used_at timestamptz,
    used_by uuid REFERENCES users (id) ON DELETE SET NULL
);

-- Email verification and password reset links.
CREATE TABLE email_tokens (
    digest bytea PRIMARY KEY CHECK (length(digest) = 32),
    user_id uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    purpose text NOT NULL CHECK (purpose IN ('verify', 'reset')),
    expires_at timestamptz NOT NULL,
    used_at timestamptz
);
CREATE INDEX email_tokens_user ON email_tokens (user_id);

CREATE TABLE sessions (
    digest bytea PRIMARY KEY CHECK (length(digest) = 32),
    user_id uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    last_seen_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL
);
CREATE INDEX sessions_user ON sessions (user_id);

-- Where an account last signed in successfully: such a (user, address) pair skips the
-- global sign-in budget, so a distributed flood cannot lock the owner out (signin.rs).
CREATE TABLE sign_in_trust (
    user_id uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    address text NOT NULL CHECK (length(address) BETWEEN 1 AND 64),
    last_success_at timestamptz NOT NULL,
    PRIMARY KEY (user_id, address)
);

CREATE TABLE workspaces (
    id uuid PRIMARY KEY,
    name text NOT NULL CHECK (length(name) BETWEEN 1 AND 100),
    plan text NOT NULL DEFAULT 'free' CHECK (plan IN ('free')),
    created_by uuid REFERENCES users (id) ON DELETE SET NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE members (
    workspace_id uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    user_id uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    role text NOT NULL CHECK (role IN ('owner', 'admin', 'member')),
    added_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, user_id)
);
CREATE INDEX members_user ON members (user_id);

CREATE TABLE api_tokens (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    prefix text NOT NULL UNIQUE CHECK (prefix ~ '^[a-z2-7]{12}$'),
    digest bytea NOT NULL UNIQUE CHECK (length(digest) = 32),
    name text NOT NULL CHECK (length(name) BETWEEN 1 AND 100),
    created_by uuid REFERENCES users (id) ON DELETE SET NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    last_used_at timestamptz
);
CREATE INDEX api_tokens_workspace ON api_tokens (workspace_id);

-- The security log: what happened to which account or workspace, never an email, a name
-- or an address, so a deleted account's events refer to an id that resolves to nobody.
-- Append-only, even for a runtime role that tried; pruned only by the owner
-- (`xpayctl prune-events`).
CREATE TABLE account_events (
    id uuid PRIMARY KEY,
    at timestamptz NOT NULL,
    kind text NOT NULL CHECK (kind ~ '^[a-z_.]{1,64}$'),
    user_id uuid,
    workspace_id uuid
);
CREATE INDEX account_events_at ON account_events (at);

CREATE TRIGGER account_events_append_only BEFORE UPDATE OR DELETE ON account_events
    FOR EACH ROW EXECUTE FUNCTION refuse_change();
CREATE TRIGGER account_events_no_truncate BEFORE TRUNCATE ON account_events
    FOR EACH STATEMENT EXECUTE FUNCTION refuse_change();
CREATE TRIGGER account_events_stamp BEFORE INSERT ON account_events
    FOR EACH ROW EXECUTE FUNCTION stamp_event();
-- ALWAYS: session_replication_role = replica does not silence them either.
ALTER TABLE account_events ENABLE ALWAYS TRIGGER account_events_append_only;
ALTER TABLE account_events ENABLE ALWAYS TRIGGER account_events_no_truncate;
ALTER TABLE account_events ENABLE ALWAYS TRIGGER account_events_stamp;

GRANT SELECT, INSERT, UPDATE, DELETE
    ON users, invites, email_tokens, sessions, sign_in_trust, workspaces, members, api_tokens
    TO xpay_app;
GRANT SELECT, INSERT ON account_events TO xpay_app;
