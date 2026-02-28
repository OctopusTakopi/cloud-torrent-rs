#!/bin/bash
echo "Total args: $#"
for arg in "$@"; do
  echo "Arg: '$arg'"
done
