#!/usr/bin/env bash
# Test: C-style for ((init; cond; step)) and break/continue

# Basic C-style for
for ((i = 0; i < 5; i++)); do
    echo -n "$i "
done
echo

# For with step > 1
for ((i = 0; i <= 20; i += 5)); do
    echo -n "$i "
done
echo

# For counting down
for ((i = 10; i > 0; i--)); do
    echo -n "$i "
done
echo

# Continue inside arithmetic for
for ((i = 0; i < 6; i++)); do
    if ((i % 2 == 0)); then
        continue
    fi
    echo -n "$i "
done
echo

# Break inside arithmetic for
for ((i = 0; i < 100; i++)); do
    if ((i == 7)); then
        break
    fi
    echo -n "$i "
done
echo

# Nested arithmetic for with break n
for ((i = 1; i <= 3; i++)); do
    for ((j = 1; j <= 3; j++)); do
        if ((i == 2 && j == 2)); then
            break 2
        fi
        echo -n "${i},${j} "
    done
    echo "|"
done
echo "done"

# Prefix/postfix increment semantics
a=0
b=$((++a))
echo "a=$a b=$b"
c=$((a++))
echo "a=$a c=$c"
