#!/usr/bin/env sh
# Download the Super User Stack Exchange dump (~1.3 GB) and turn its posts
# into data/superuser.docs.txt (one document per line, ~0.9 GB, 1.24M docs).
# Needs curl, 7z (p7zip-full) and python3.
set -eu
cd "$(dirname "$0")/.."
mkdir -p data
cd data
[ -f superuser.com.7z ] || curl -fL --retry 4 -o superuser.com.7z \
  https://archive.org/download/stackexchange/superuser.com.7z
[ -f Posts.xml ] || 7z e -y superuser.com.7z Posts.xml
python3 ../scripts/prepare_stackexchange.py Posts.xml superuser.docs.txt
