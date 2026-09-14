# Connecting things to the engine

One `ns-app serve` process hosts many companies, and each company can be
reached four ways at once. A customer with a browser tab open, the same
customer's desktop app, and that customer messaging the company on WhatsApp
are **one conversation** — the engine is handed the same session id for all
of them and has no field with which to ask which was which.

| way in | who it is for | port | reply arrives |
|---|---|---|---|
| TCP, one JSON object per line | desktop apps, scripts, anything that can hold a socket | `[serve] listen` | on the same socket |
| `GET /chat`, upgraded to a WebSocket | a custom web chat window, and desktop apps that would rather speak HTTP | `[http] listen` | on the same socket |
| `POST /v1/messages` | `curl`, a cron job, another back end | `[http] listen` | in the response |
| `POST /hooks/<platform>` | WhatsApp, and platforms shaped like it | `[http] listen` | as a call to the platform's API |

Everything below the transport is shared: one queue per company, one engine
per company, one set of windows per session (`ns-channel-hub`). Adding a way
in does not add an engine, a store, or a second copy of a conversation.

---

## 0. Setting it up in a page

Everything below can be done in a form instead:

```sh
ns-app admin          # prints a loopback link carrying a one-time token
```

Three tabs. **Process** is what the whole shard shares — provider, models,
the two listen addresses. **Companies** adds one and edits its persona, its
database and its keys. **Credentials** is every variable your configuration
names, whether each has a value, and a box to give it one.

Two rules it keeps, and they are the reason it is worth having rather than a
generic TOML editor:

- **A key never goes into the config.** The config names variables — that is
  what keeps a credential out of a file somebody commits — so the page writes
  *values* to a `.env` beside it and leaves the naming where it was. It adds
  `.env` to `.gitignore` the first time it runs.
- **A key never comes back out.** The page is told a variable's name, what
  the config uses it for, and whether it is set. Never the value.

It edits the files in place with their comments intact, and every save has to
parse as a config before it is written — so a mistake is a refusal on screen
rather than a process that will not start next time. It binds loopback only,
with no flag to say otherwise: it edits credentials and has no TLS. Forward a
port if you need it from elsewhere.

Changes land in the files; a running `ns-app serve` is not reconfigured under
itself, so restart it to pick them up.

The rest of this section is the same setup done by hand, which is also what
the page writes.

## 0.1 A company to try it with

A company is a file in `tenants/`, named for the id it is addressed by. This
is the whole of one:

```toml
# tenants/acme.toml
[persona]
text = "You are Acme's support assistant. Be brief and concrete."

[store]
path = "ns-acme.sqlite"          # its own database; isolation is the filesystem's

[auth]
signing_key_envs = ["NS_SIGNING_KEY_ACME"]   # named, never held in the file
```

and beside it, in `config.toml`, the two ways in the process opens:

```toml
[serve]
listen = "127.0.0.1:7375"
auth = "jwt"                     # the chat window presents a token and nothing else

[http]
listen = "127.0.0.1:8787"
origins = ["*"]                  # a page opened from a file has no origin
```

Then:

```sh
export NS_SIGNING_KEY_ACME="something long and random"
ns-app serve
TOKEN=$(ns-app token acme --subject martin)
```

Nothing is built until someone speaks: the first message for `acme` is what
opens its database and builds its engine. Its sessions are `acme/web/martin`
and the like — the company is the first segment, so two companies cannot
collide, and a token for one cannot name a session of the other.

To see it: serve the example page and open it against the endpoint.

```sh
cd crates/channel-http/examples && python -m http.server 8080
# then: http://127.0.0.1:8080/web-chat.html?ws=127.0.0.1:8787
```

Paste the token, and talk. (Opening the page straight off disk works too —
it falls back to `127.0.0.1:8787` — but serving it is closer to how it will
really be used, and avoids each browser's rules about what a `file://` page
may do.)

---

## 1. The wire, once

The socket and the WebSocket speak the same three objects, so one client
library serves both and changing transport is changing how you connect.

```jsonc
// first, from the client — the hello
{"token": "<credential>", "session": "<ignored unless auth = shared>"}
// then, from the client, one per message
{"text": "when does my order ship?"}
// from the server, one per reply
{"session": "acme/web/u1", "text": "Tomorrow morning."}
```

**The session is the server's to decide, never the client's to claim.** Under
`auth = "jwt"` the session id is derived from the credential
(`<tenant>/<channel>/<subject>`) and the `session` field is ignored. A client
cannot name itself into somebody else's conversation, and a second window
presenting the same credential *joins* the conversation rather than
displacing the first.

A refused hello is closed with nothing explained — a caller that failed it
learns only that it failed. The log line says which refusal it was.

## 2. A desktop app, or anything with a socket

```
$ nc 127.0.0.1 7375
{"token":"eyJhbGci...","session":null}
{"text":"hello"}
{"session":"acme/web/u1","text":"Hello — how can I help?"}
```

One JSON object per line, UTF-8, `\n`-terminated, both ways. EOF ends the
connection. A line that is not one of those objects is ignored with a log
line, so one bad frame does not cost a conversation.

This is the oldest way in and it needs no HTTP endpoint: `[serve]` alone.

## 3. A custom web chat window

A browser cannot open a TCP socket, so a web widget upgrades instead. The
hello is the **first text frame**, under the same deadline the socket puts on
its first line.

```js
const ws = new WebSocket("wss://chat.example.com/chat");
ws.onopen = () => ws.send(JSON.stringify({ token: TOKEN }));
ws.onmessage = (e) => render(JSON.parse(e.data).text);
send.onclick = () => ws.send(JSON.stringify({ text: input.value }));
```

A complete page, with the reconnect and the close codes handled, is
`crates/channel-http/examples/web-chat.html` — about a hundred lines, meant
to be read and thrown away rather than served from here.

**Where the token comes from.** The company's own back end mints it: an
HS256 JWT signed with that company's signing key, `iss` the tenant id, `sub`
an opaque per-user subject (never an email address or a phone number), with
`iat` and `exp` set. The browser never holds the signing key. Close codes:
`4001` the hello was not accepted, `1009` a frame or message past its bound,
`1001` the shard is shutting down.

Before that back end exists — a test company, a staging box, someone wanting
to see whether any of this works — `ns-app` will mint one:

```sh
ns-app token acme --subject martin --minutes 120
```

It reads that company's *current* signing key from the variable its
`[auth] signing_key_envs` names, and prints the token and nothing else, so
`TOKEN=$(ns-app token acme)` is the whole of using it. It refuses a company
running `auth = "shared"`, which verifies no token, and refuses a subject the
session id cannot be made of. It is not part of the production path: there,
the company mints its own, in its own service, at the moment it knows which
of *its* users is on the page.

**Cross-origin.** `[http] origins` lists the browser origins allowed to open
a window or call the JSON route; `["*"]` is the right answer for a widget
embedded on customer sites whose domains this shard has never been told
about. The credential is what keeps a caller out — the origin list is what
stops a page elsewhere from quietly spending a visitor's token.

**Keepalive.** The server pings an open window every 25 seconds, inside the
sixty most proxies reap an idle connection after, so a quiet conversation is
not dropped between turns. A client that wants to notice a dead server sooner
can ping as well; this channel answers pongs.

## 4. One message, one reply

For a script, a cron job, or a back end putting its own UI in front of the
engine:

```
$ curl -s https://chat.example.com/v1/messages \
    -H "authorization: Bearer $TOKEN" \
    -d '{"text":"when does my order ship?"}'
{"session":"acme/web/u1","text":"Tomorrow morning."}
```

It is the same conversation as the window's, not a second kind: a reply also
reaches any chat window holding that session. A turn that outlasts
`[http] reply_timeout_ms` answers `504` with the session id — the turn is
still running, and its answer is in the log and on any window that is open.

## 5. A platform, over webhooks

Webhooks are the right shape here and not a second-best one. A platform does
not hold a connection open: it posts when its user says something, wants
`200` in milliseconds, and expects the reply as a call to its own API some
seconds later. Three things follow, and each is handled at the edge:

1. **The sender's id is not a credential.** Anyone can write a phone number
   into a JSON body. What is verified is the HMAC-SHA256 over the **raw**
   bytes — before the body is parsed, because a body that has been through a
   JSON round trip will not hash. The company is then taken from the business
   account id *inside that verified payload*, never from a header, a query
   parameter or anything the sender chose.
2. **A retry must not run the turn twice.** Platforms retry anything they did
   not get a `200` for (Meta, for up to seven days) and may batch several
   messages into one delivery. Message ids are remembered in
   `[http] seen_path`, so a redelivery after a restart is still a redelivery.
   A batch is sorted by platform timestamp before anything is enqueued.
3. **There is no socket to reply on.** The way back is a task holding a hub
   sink and calling the platform's API with the company's own access token,
   retried three times and then logged.

### WhatsApp, end to end

```toml
# config.toml — the endpoint is the process's
[http]
listen = "127.0.0.1:8787"
whatsapp_verify_token_env = "NS_WA_VERIFY_TOKEN"
seen_path = "ns-webhooks-seen.tsv"

# tenants/acme.toml — the account is the company's
[whatsapp]
phone_number_id = "123456789012345"
access_token_env = "NS_WA_TOKEN_ACME"
app_secret_env   = "NS_WA_SECRET_ACME"
session_salt_env = "NS_WA_SALT_ACME"
```

Point Meta at `https://<your host>/hooks/whatsapp`. It will `GET` it once
with `hub.challenge`, which is answered only if `hub.verify_token` matches;
after that it `POST`s deliveries signed with the app secret.

Two companies on one endpoint are separated by the business number inside
their own signed payloads, and a payload signed with one company's secret
that names another company's number is refused — otherwise any company on
the shard could speak as any other.

**A phone number never becomes a session id.** The subject is
`HMAC(that company's salt, customer id)`, so the id the engine sees, stores
facts under and writes into its log is opaque, and the same customer at two
companies is two unrelated subjects. The number itself exists only in the
reply path, in memory, while the conversation is live.

**The send window.** Meta allows free-form business messages only inside a
window after the customer's last message (24 hours at the time of writing);
outside it a pre-approved template is required. Every reply sent here answers
a message that just arrived, so it is inside the window by construction.
Anything proactive — a follow-up, a nudge, a scheduled message — is not, and
needs the template API before it is built.

### Another platform

One `impl Platform` is the whole of it: `accept` (verify over raw bytes, then
read the messages and the company out of the verified payload), `reply` (the
API call), and optionally `verification` for a platform that does a
challenge handshake first. `crates/channel-http/src/whatsapp.rs` is the
worked example; the trait is in `platform.rs`. A new adapter needs no change
to the engine, the hub, or any other channel.

## 6. Configuration, in one place

```toml
[serve]                       # the socket, as before
listen = "127.0.0.1:7375"
auth = "jwt"                  # "shared" is loopback-only development
token_env = "NS_SERVE_TOKEN"
max_connections = 8

[http]                        # the HTTP endpoint; empty listen = not opened
listen = "127.0.0.1:8787"
allow_remote = false
max_connections = 256
hello_timeout_ms = 5000
reply_timeout_ms = 120000
max_body_bytes = 262144
origins = ["https://shop.example"]   # or ["*"]
seen_path = "ns-webhooks-seen.tsv"
whatsapp_verify_token_env = "NS_WA_VERIFY_TOKEN"

[auth]                        # per company, in tenants/<id>.toml
signing_key_envs = ["NS_SIGNING_KEY_ACME"]
iat_floor = 0
```

`[serve]`, `[http]` and `[models]` are process-owned: a tenant overlay that
sets one is refused by name at startup, because one port belongs to the
process however many companies arrive on it.

## 7. What is deliberately not here

- **TLS.** Terminate it in front — a reverse proxy, or the platform's own
  tunnel. A non-loopback bind is refused unless `allow_remote` says it was
  meant, and a credential scheme that proves nothing (`auth = "shared"`) is
  refused off loopback whatever else is configured.
- **A UI.** The example page is an example.
- **Sessions of the channel's own.** Who a caller is belongs to
  `ns-identity`; a channel knows only that something turned a hello, a bearer
  token or a signature into an identity.
