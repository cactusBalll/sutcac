#!/usr/bin/env bash
# Test: for loops over words

# Basic for
for x in a b c; do
    echo -n "$x "
done
echo

# For with word splitting
items="one two three"
for y in $items; do
    echo -n "$y "
done
echo

# For default to $@
loop_args() {
    for arg; do
        echo "[$arg]"
    done
}
loop_args x y z

# For with command substitution
for n in $(seq 1 3); do
    echo -n "$n "
done
echo

# For with glob
touch for_a.txt for_b.txt
for f in for_*.txt; do
    echo "$f"
done
rm -f for_a.txt for_b.txt

# Empty word list
for empty in; do
    echo "should not print"
done
echo "done"
