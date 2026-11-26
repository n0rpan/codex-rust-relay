#!/bin/sh
while IFS= read -r row; do
  printf '%s\n' "$row" >> wire
  case "$row" in
    *'"request_id":"ox-init"'*) key=init ;;
    *'"type":"user"'*) key=$(printf '%s' "$row" | sed -n 's/.*"uuid":"\([^"]*\)".*/\1/p') ;;
    *) key= ;;
  esac
  [ -n "$key" ] && [ -f "after.$key" ] && cat "after.$key"
done
