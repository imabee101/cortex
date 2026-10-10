pub fn esc(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn layout(title: &str, body: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="robots" content="noindex">
<title>{title}</title>
<style>
:root {{
  --s: 8px;
  --ink: #1c1917;
  --paper: #f4f1ea;
  --muted: #57534e;
  --line: #d6d3d1;
  --accent: #0f6e56;
  --accent-ink: #f7f6f3;
  --danger: #9f1239;
}}
* {{ box-sizing: border-box; }}
body {{
  margin: 0 auto;
  max-width: 65ch;
  padding: calc(var(--s) * 5) calc(var(--s) * 3);
  color: var(--ink);
  background: var(--paper);
  font-family: "Source Sans 3", "Nimbus Sans", "Liberation Sans", sans-serif;
  font-size: 18px;
  line-height: 1.45;
}}
h1 {{
  margin: 0 0 calc(var(--s) * 2);
  font-size: 28px;
  letter-spacing: 0.04em;
}}
p {{ margin: 0 0 calc(var(--s) * 2); color: var(--muted); }}
form {{ display: flex; flex-direction: column; gap: calc(var(--s) * 2); }}
label {{ display: flex; flex-direction: column; gap: var(--s); font-weight: 600; }}
input {{
  padding: calc(var(--s) * 1.5) calc(var(--s) * 2);
  border: 1px solid var(--line);
  background: #fff;
  color: var(--ink);
  font: inherit;
}}
.row {{ display: flex; gap: calc(var(--s) * 2); }}
button {{
  padding: calc(var(--s) * 2);
  border: 0;
  background: var(--accent);
  color: var(--accent-ink);
  font: inherit;
  font-weight: 700;
  cursor: pointer;
}}
button.deny {{ background: transparent; color: var(--danger); border: 1px solid var(--danger); }}
.err {{ color: var(--danger); }}
a {{ color: var(--accent); }}
code {{ font-family: "IBM Plex Mono", "Nimbus Mono PS", monospace; overflow-wrap: anywhere; }}
pre {{ margin: 0 0 calc(var(--s) * 2); padding: calc(var(--s) * 2); background: #fff; border: 1px solid var(--line); white-space: pre-wrap; overflow-wrap: anywhere; }}
.secret {{ display: block; padding: calc(var(--s) * 2); background: #fff; border: 2px solid var(--accent); color: var(--ink); }}
table {{ width: 100%; border-collapse: collapse; margin: 0 0 calc(var(--s) * 3); }}
th, td {{ padding: var(--s); border-bottom: 1px solid var(--line); text-align: left; vertical-align: middle; }}
th {{ font-size: 15px; color: var(--muted); font-weight: 600; }}
td form {{ display: inline; }}
td button {{ padding: var(--s) calc(var(--s) * 2); }}
h2 {{ margin: calc(var(--s) * 4) 0 calc(var(--s) * 2); font-size: 20px; }}
</style>
</head>
<body>
<h1>Cortex</h1>
{body}
</body>
</html>"#,
        title = esc(title),
    )
}

pub fn login_page(next: &str, error: Option<&str>) -> String {
    let err = error
        .map(|e| format!(r#"<p class="err">{}</p>"#, esc(e)))
        .unwrap_or_default();
    layout(
        "Sign in",
        &format!(
            r#"{err}
<p>Sign in to continue to Cortex.</p>
<form method="post" action="/login">
<input type="hidden" name="next" value="{next}">
<label>Email <input name="email" type="email" autocomplete="username" required></label>
<label>Password <input name="password" type="password" autocomplete="current-password" required></label>
<button type="submit">Sign in</button>
</form>
<p><a href="/register?next={next}">Create an account</a></p>"#,
            next = esc(next),
        ),
    )
}

pub fn register_page(next: &str, error: Option<&str>) -> String {
    let err = error
        .map(|e| format!(r#"<p class="err">{}</p>"#, esc(e)))
        .unwrap_or_default();
    layout(
        "Create account",
        &format!(
            r#"{err}
<p>Create an account. You still confirm access on the next screen.</p>
<form method="post" action="/register">
<input type="hidden" name="next" value="{next}">
<label>Email <input name="email" type="email" autocomplete="username" required></label>
<label>Given name <input name="first_name" autocomplete="given-name" required></label>
<label>Family name <input name="last_name" autocomplete="family-name" required></label>
<label>Password <input name="password" type="password" autocomplete="new-password" minlength="12" required></label>
<button type="submit">Create account</button>
</form>"#,
            next = esc(next),
        ),
    )
}

pub fn consent_page(email: &str, state: &str, scope: &str) -> String {
    layout(
        "Allow access",
        &format!(
            r#"<p>{email} is about to allow this sign-in.</p>
<p>Requested access: <code>{scope}</code></p>
<form method="post" action="/consent">
<input type="hidden" name="state" value="{state}">
<div class="row">
<button type="submit" name="decision" value="allow">Allow</button>
<button class="deny" type="submit" name="decision" value="deny">Deny</button>
</div>
</form>"#,
            email = esc(email),
            state = esc(state),
            scope = esc(scope),
        ),
    )
}

pub fn device_page(user_code: &str, signed_in: bool, next: &str) -> String {
    if !signed_in {
        return layout(
            "Device sign-in",
            &format!(
                r#"<p>Sign in, then confirm the device code.</p>
<form method="post" action="/login">
<input type="hidden" name="next" value="{next}">
<label>Email <input name="email" type="email" required></label>
<label>Password <input name="password" type="password" required></label>
<button type="submit">Sign in</button>
</form>"#,
                next = esc(next),
            ),
        );
    }
    layout(
        "Confirm device",
        &format!(
            r#"<p>Confirm this device code before the CLI can continue.</p>
<form method="post" action="/device/decide">
<label>Device code <input name="user_code" value="{code}" autocomplete="one-time-code" required></label>
<div class="row">
<button type="submit" name="decision" value="approve">Approve</button>
<button class="deny" type="submit" name="decision" value="deny">Deny</button>
</div>
</form>"#,
            code = esc(user_code),
        ),
    )
}

pub fn message_page(title: &str, text: &str) -> String {
    layout(title, &format!("<p>{}</p>", esc(text)))
}

/// What the API key console reports above the key list.
pub enum KeyNotice<'a> {
    None,
    /// The plaintext of a key just created; this is the only time it is shown.
    Created(&'a str),
    Revoked,
    Error(&'a str),
}

fn when(at: chrono::DateTime<chrono::Utc>) -> String {
    at.format("%Y-%m-%d %H:%M UTC").to_string()
}

pub fn api_keys_page(keys: &[crate::db::ApiKey], csrf: &str, notice: KeyNotice<'_>) -> String {
    let notice = match notice {
        KeyNotice::None => String::new(),
        KeyNotice::Created(key) => format!(
            r#"<h2>New key</h2>
<p>Copy this key now. It is not shown again.</p>
<code class="secret">{key}</code>
<p>Use it wherever Cortex runs without a browser:</p>
<pre><code>export CORTEX_API_KEY="{key}"</code></pre>"#,
            key = esc(key),
        ),
        KeyNotice::Revoked => "<p>Key revoked. Requests that use it now fail.</p>".to_owned(),
        KeyNotice::Error(text) => format!(r#"<p class="err">{}</p>"#, esc(text)),
    };
    let list = if keys.is_empty() {
        "<p>No keys yet.</p>".to_owned()
    } else {
        let rows: String = keys
            .iter()
            .map(|key| {
                format!(
                    r#"<tr><td>{name}</td><td><code>{prefix}…{suffix}</code></td><td>{created}</td><td>{used}</td><td><form method="post" action="/account/api-keys/revoke"><input type="hidden" name="csrf" value="{csrf}"><input type="hidden" name="id" value="{id}"><button class="deny" type="submit">Revoke</button></form></td></tr>"#,
                    name = esc(&key.name),
                    prefix = crate::API_KEY_PREFIX,
                    suffix = esc(&key.key_suffix),
                    created = when(key.created_at),
                    used = key.last_used_at.map(when).unwrap_or_else(|| "Never".to_owned()),
                    csrf = esc(csrf),
                    id = key.id,
                )
            })
            .collect();
        format!(
            "<table><thead><tr><th>Name</th><th>Key</th><th>Created</th><th>Last used</th><th>Actions</th></tr></thead><tbody>{rows}</tbody></table>"
        )
    };
    layout(
        "API keys",
        &format!(
            r#"<p>An API key lets Cortex run without a browser sign-in, for scripts, CI, and containers. Anyone holding a key acts as your account, so keep it secret and revoke it when you no longer need it.</p>
{notice}
<h2>Your keys</h2>
{list}
<h2>Create a key</h2>
<form method="post" action="/account/api-keys">
<input type="hidden" name="csrf" value="{csrf}">
<label>Name <input name="name" maxlength="64" placeholder="ci-runner" required></label>
<button type="submit">Create key</button>
</form>"#,
            csrf = esc(csrf),
        ),
    )
}
