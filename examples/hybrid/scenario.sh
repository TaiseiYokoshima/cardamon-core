#!/bin/sh
for i in $(seq 1 1500); do
  curl localhost:4243/rand >/dev/null 2>&1
  curl localhost:4244/rand >/dev/null 2>&1
done
