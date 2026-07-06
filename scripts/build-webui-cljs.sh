#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/../webui"
npm run release
cp resources/cljs/app.js resources/app.js
