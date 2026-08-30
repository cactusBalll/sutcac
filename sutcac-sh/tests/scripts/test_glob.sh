#!/usr/bin/env bash
# Test: glob expansion

# Create files
touch file1.txt file2.txt file_a.log other.dat

# Star glob
for f in file*.txt; do echo "$f"; done

# Question mark glob
for f in file?.txt; do echo "$f"; done

# Character class glob
for f in file[12].txt; do echo "$f"; done

# Glob no match remains literal
for f in nomatch*.xyz; do echo "literal=$f"; done

# Glob protected by quotes
echo '*.txt'
echo "*.txt"

# Mixed word with glob
prefix=pre
touch pre1.txt pre2.txt
for f in ${prefix}*.txt; do echo "$f"; done

# Cleanup
rm -f file1.txt file2.txt file_a.log other.dat pre1.txt pre2.txt
