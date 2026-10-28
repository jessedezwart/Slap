#!/bin/sh
# Starts as root only to adopt the host owner of the bind-mounted secrets/,
# so yt-dlp can write the cookie jar back without anyone having to chown.
set -eu

if [ "$(id -u)" != 0 ]; then
    exec "$@"
fi

uid=$(stat -c %u /app/secrets)
gid=$(stat -c %g /app/secrets)
# Root-owned means a named volume or a dir Docker created: use the bot user.
if [ "$uid" = 0 ]; then
    uid=$(id -u bot)
    gid=$(id -g bot)
fi

# The cache volume and yt-dlp/deno caches must follow whoever we run as.
for dir in /app/data /home/bot; do
    [ "$(stat -c %u:%g "$dir")" = "$uid:$gid" ] || chown -R "$uid:$gid" "$dir"
done

export HOME=/home/bot
exec setpriv --reuid="$uid" --regid="$gid" --clear-groups -- "$@"
