#!/usr/bin/env bash
# Test: command substitution $(...)

# Basic substitution
echo "$(echo hello)"

# Substitution producing multiple words
words=$(echo a b c)
for w in $words; do echo "[$w]"; done

# Nested substitution
outer=$(echo inner=$(echo value))
echo "$outer"

# Substitution in arithmetic
n=$(echo 5)
echo $((n * 2))

# Substitution in variable assignment
x=$(printf '1\n2\n3')
echo "$x"

# Substitution in pipeline
files=$(echo README.md)
echo "$files" | wc -w
