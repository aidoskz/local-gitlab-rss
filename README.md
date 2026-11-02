# local-gitlab-rss

A small Atom/RSS feed reader that sends new items to Telegram. It was made for
GitLab activity feeds, so it has regex filters for things like "only tags and
pushes to `main`/`master`".

- Single static binary, no runtime dependencies (besides system CA certificates)
- Atom and RSS 2.0 feeds
- Messages go to a Telegram chat, group or forum topic, with a link to the item
- Include/exclude filters on item titles, per feed
- Items that fail to send are retried on the next check

## Quick start

```bash
cargo build --release
cp .env.example .env               # then put your bot token in it
./target/release/local-gitlab-rss telegram
./target/release/local-gitlab-rss add 'https://gitlab.example.com/group/project.atom?feed_token=...'
./target/release/local-gitlab-rss run
```

### Telegram setup

1. Message [@BotFather](https://t.me/BotFather), send `/newbot`, copy the token.
2. Put it in `.env` as `TELEGRAM_TOKEN=...`.
3. Send any message to your new bot (or add it to a group and post there).
4. Run `local-gitlab-rss telegram`. It finds the chat from that message, sends a
   confirmation and saves the chat id to `data.json`.

### GitLab feed URL

On a GitLab project or group page, click the RSS icon ("Subscribe to RSS feed")
and copy the link. It contains your personal `feed_token`. If the reader
says it got an HTML page instead of a feed, the token is wrong or expired:
reset it under **User Settings → Access Tokens → Feed token** and add the
feed again.

Quote the URL in the shell (`'...'`) so `?` and `&` aren't interpreted.

## Commands

| Command | Description |
|---|---|
| `telegram [chat_id]` | Connect Telegram. Without `chat_id`, uses the last chat that messaged the bot |
| `test` | Send a test message |
| `add <url>` | Subscribe. Items already in the feed are marked as seen, so you only get new ones |
| `remove <feed>` | Unsubscribe |
| `list` | Show feeds (numbered) and their filters |
| `filter <feed> include <regex>` | Only send items whose title matches (any include) |
| `filter <feed> exclude <regex>` | Never send items whose title matches |
| `filter <feed> clear` | Remove all filters of a feed |
| `preview <feed>` | Fetch the feed and show which current items would be sent |
| `check` | Check all feeds once |
| `run [minutes]` | Check in a loop, every 10 minutes by default |

`<feed>` is the number from `list` or the full feed URL.

## Filters

Filters are [regular expressions](https://docs.rs/regex/latest/regex/#syntax)
matched against the item title. An item is sent when it matches at least one
`include` (or there are none) and no `exclude`. Matching is case-sensitive;
prefix a pattern with `(?i)` to ignore case. Filtered-out items are marked as
seen.

GitLab activity titles look like this:

```
Jane Doe pushed new project tag 1.4.0 at Group / project
Jane Doe pushed to project branch master at Group / project
Jane Doe accepted merge request !42: Fix login at Group / project
Jane Doe opened merge request !42: Fix login at Group / project
Jane Doe pushed new project branch feature/x at Group / project
Jane Doe deleted project branch feature/x at Group / project
```

Example: tags, pushes to `main`/`master` and merged MRs, but not a deploy bot:

```bash
local-gitlab-rss filter 1 include 'pushed new project tag'
local-gitlab-rss filter 1 include 'pushed to project branch (main|master) '
local-gitlab-rss filter 1 include 'accepted merge request'
local-gitlab-rss filter 1 exclude '^Deployment Bot'
local-gitlab-rss preview 1
```

The trailing space after `(main|master)` keeps branches like `master-old` out.
Other useful patterns: `at Group / project$` (one project), `^Jane Doe` (one
author).

## Configuration

Settings come from environment variables or from a `.env` file in the data
directory. Real environment variables take precedence.

| Variable | Description |
|---|---|
| `TELEGRAM_TOKEN` | Bot token from @BotFather (required) |
| `TELEGRAM_CHAT_ID` | Chat to send to. Optional: `local-gitlab-rss telegram` saves it to `data.json` |
| `TELEGRAM_THREAD_ID` | Forum topic id, for groups with topics enabled (optional) |
| `LOCAL_GITLAB_RSS_DIR` | Directory with `data.json` and `.env`. Defaults to the project directory at build time |

For a forum topic, the topic id is the middle number in a message link
(`https://t.me/c/<chat>/<thread_id>/<message>`); `TELEGRAM_CHAT_ID` must then
be the group id (`-100...`).

State is kept in `data.json`: subscribed feeds, their filters, the ids of
already-seen items and the saved chat id. It is written with `0600`
permissions because feed URLs contain tokens. `.env` and `data.json` are in
`.gitignore`; don't commit them.

## Building for other platforms

```bash
make                # macOS (native) + Linux amd64 + Linux arm64
make mac            # macOS, current architecture
make linux-amd64    # Linux x86_64 (most VPS)
make linux-arm64    # Linux ARM
make clean
```

Binaries are written to `dist/`. Linux builds run in a `rust:alpine` Docker
container and are statically linked against musl, so they run on any distro
without glibc version issues.

## Running on a Linux server

```bash
scp dist/local-gitlab-rss-linux-amd64 server:~/local-gitlab-rss/local-gitlab-rss
scp .env data.json server:~/local-gitlab-rss/
```

On the server:

```bash
sudo apt install ca-certificates   # usually already installed; needed for HTTPS
cd ~/local-gitlab-rss
chmod +x local-gitlab-rss && chmod 600 .env data.json
export LOCAL_GITLAB_RSS_DIR=~/local-gitlab-rss
./local-gitlab-rss list
./local-gitlab-rss run
```

`LOCAL_GITLAB_RSS_DIR` is required here, because a binary built elsewhere
points at the build machine's project directory by default.

To keep it running, use a systemd service, e.g.
`/etc/systemd/system/local-gitlab-rss.service`:

```ini
[Unit]
Description=local-gitlab-rss
After=network-online.target
Wants=network-online.target

[Service]
User=youruser
Environment=LOCAL_GITLAB_RSS_DIR=/home/youruser/local-gitlab-rss
ExecStart=/home/youruser/local-gitlab-rss/local-gitlab-rss run 5
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now local-gitlab-rss
journalctl -u local-gitlab-rss -f
```
