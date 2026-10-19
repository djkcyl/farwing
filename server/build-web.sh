#!/bin/sh
# Build Farwing Server with its web console embedded: the front end first
# (server/web -> server/web/dist), then the server binary that includes it.
#
#   sh server/build-web.sh               # release build
#   sh server/build-web.sh --skip-cargo  # front end only
set -eu
server=$(cd "$(dirname "$0")" && pwd)

cd "$server/web"
npm ci
npm run check
npm run build

if [ "${1:-}" != "--skip-cargo" ]; then
    cd "$server"
    cargo build --release --locked
    echo "farwing-server with the web console: $server/target/release/farwing-server"
fi
