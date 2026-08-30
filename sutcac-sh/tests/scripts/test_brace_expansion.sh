#!/usr/bin/env bash
# Test: brace expansion {a,b,c}, {1..10}, {a..z}, {1..10..2}, nested

# Simple comma brace
echo {apple,banana,cherry}

# Numeric range
echo {1..5}

# Alphabetic range lowercase
echo {a..e}

# Alphabetic range uppercase
echo {E..A}

# Range with step
echo {0..10..2}

# Brace as part of word
echo file.{txt,md,rst}

# Nested brace
echo {a,{b,c},d}

# Multiple ranges in one word
echo {x..z}{1..3}

# Empty item in brace
echo {foo,bar,}

# Brace protected by single quotes should NOT expand
echo '{not,expanded}'

# Brace protected by double quotes should NOT expand
echo "{also,not}"

# Mixed quoted/unquoted segments
prefix=pre
echo ${prefix}-{1,2,3}

# Complex nested with ranges
echo {a..c}{1..2}

# Suffix preservation
echo img{100..102}.png

# Negative step range
echo {5..1}
