#!/usr/bin/env bash
# Test: while loops

# Basic while
i=0
while (( i < 5 )); do
    echo -n "$i "
    ((i++))
done
echo

# While with break
j=0
while true; do
    if (( j >= 3 )); then
        break
    fi
    echo -n "$j "
    ((j++))
done
echo

# While with continue
k=0
while (( k < 6 )); do
    ((k++))
    if (( k % 2 == 0 )); then
        continue
    fi
    echo -n "$k "
done
echo

# While iterating over word list via for-like pattern
items="alpha beta gamma"
n=0
for line in $items; do
    n=$((n + 1))
    echo "$n:$line"
done
