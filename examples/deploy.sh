#!/usr/bin/env bash
set -euo pipefail

target="${1:-staging}"
echo "Deploying to $target"

if [[ "$target" == "production" ]]; then
  read -r -p "Deploy to production? [y/N] " answer
  [[ "$answer" == "y" ]] || exit 1
fi

rsync -az --delete ./dist/ "deploy@$target.example.com:/srv/app/"
echo "Done"
