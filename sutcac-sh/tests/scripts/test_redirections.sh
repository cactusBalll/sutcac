#!/usr/bin/env bash
# Test: input/output redirections

# Output redirect >
echo "line1" > out.txt
cat out.txt

# Append >>
echo "line2" >> out.txt
cat out.txt

# Input <
echo "hello world" > in.txt
cat < in.txt

# Stderr redirect 2>
echo err 2> err.txt
cat err.txt

# Merge stderr to stdout
echo both 2>&1

# Redirect with variable target
fname=target.txt
echo data > "$fname"
cat "$fname"

# Cleanup
rm -f out.txt in.txt err.txt target.txt
