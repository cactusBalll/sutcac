#!/usr/bin/env bash
# Test: case statements

# Basic matching
for item in apple banana cherry date; do
    case $item in
        apple) echo "$item: red" ;;
        banana) echo "$item: yellow" ;;
        cherry|date) echo "$item: small" ;;
        *) echo "$item: unknown" ;;
    esac
done

# Glob patterns in case
for f in file.txt file.log file.md other.dat; do
    case $f in
        *.txt) echo "text: $f" ;;
        *.log) echo "log: $f" ;;
        file.*) echo "file.*: $f" ;;
        *) echo "other: $f" ;;
    esac
done

# Case with variable pattern
ext=md
case readme.md in
    *.$ext) echo "matches .$ext" ;;
esac
