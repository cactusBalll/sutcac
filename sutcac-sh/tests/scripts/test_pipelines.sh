#!/usr/bin/env bash
# Test: pipelines of external commands

# Basic pipeline
echo "a b c" | tr ' ' '\n' | sort

# Pipeline with wc
printf "line1\nline2\nline3\n" | wc -l

# Pipeline with grep
printf "apple\nbanana\ncherry\n" | grep 'a'

# Multi-stage pipeline
seq 1 10 | grep '[2468]' | sort -r

# Pipeline with cat
echo "hello" | cat | cat

# Command substitution in pipeline
x=$(echo 5)
seq 1 "$x" | wc -l
