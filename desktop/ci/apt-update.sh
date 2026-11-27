#!/bin/sh
# Refresh only Ubuntu's own package index. The runner image also lists
# third-party sources (Microsoft and others) that these jobs install nothing
# from, and one of them answering 403 failed a job that needed only bubblewrap.
# Extra arguments go to apt-get update.
set -eu
ubuntu=/etc/apt/sources.list.d/ubuntu.sources
if [ -f "$ubuntu" ]; then
    # List-Cleanup=0 keeps the other sources' cached lists instead of deleting them.
    set -- -o "Dir::Etc::sourcelist=$ubuntu" -o Dir::Etc::sourceparts=- -o APT::Get::List-Cleanup=0 "$@"
fi
sudo apt-get update -o Acquire::Retries=3 "$@"
