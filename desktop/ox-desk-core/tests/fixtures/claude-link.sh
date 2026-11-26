#!/bin/sh
printf '%s\n' "$*" >> launches
printf '%s\n' "$@" > args
IFS= read -r row || exit 1
printf '%s\n' "$row" > wire
cat initialize.response
while IFS= read -r row; do
  printf '%s\n' "$row" >> wire
  if [ -f disconnect-before-echo ]; then exit 0; fi
done
