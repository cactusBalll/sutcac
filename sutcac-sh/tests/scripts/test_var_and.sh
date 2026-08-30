#!/usr/bin/env bash
# Test: variable assignment combined with && / ||

a=1 && echo "a=$a"
b=2 || echo "should-not-print"
echo "b=$b"

c=3 && d=4 && echo "c=$c d=$d"

# Assignment in condition context with ! and &&
if x=5 && ! false; then
    echo "x=$x"
fi

# Chain with external command
e=hello
if echo "$e" >/dev/null && f=world; then
    echo "$e $f"
fi

# Mixed assignment and arithmetic condition
g=10
if ((g == 10)) && h=20; then
    echo "g=$g h=$h"
fi
