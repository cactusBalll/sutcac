#!/usr/bin/env bash
# Test: if ! cmd; then ... fi and if ((expr)); then ... fi

# Negation of false command
if ! false; then
    echo "negation-true"
fi

# Negation of true command
if ! true; then
    echo "negation-false-should-not-print"
else
    echo "negation-else"
fi

# Arithmetic condition true
if ((2 + 2 == 4)); then
    echo "arith-true"
fi

# Arithmetic condition false
if ((3 * 3 == 8)); then
    echo "arith-false-should-not-print"
else
    echo "arith-else"
fi

# Combined: ! ((expr))
if ! ((5 > 10)); then
    echo "not-greater"
fi

# Arithmetic with variables
x=7
if ((x % 2 == 1)); then
    echo "x-is-odd"
fi

# Nested if with arithmetic and negation
y=12
if ((y > 0)); then
    if ! ((y % 2 == 0)); then
        echo "y-positive-odd"
    else
        echo "y-positive-even"
    fi
fi

# Else-if ladder using arithmetic
n=15
if ((n < 10)); then
    echo "small"
elif ((n < 20)); then
    echo "medium"
else
    echo "large"
fi
