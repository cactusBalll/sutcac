#!/usr/bin/env bash
# Test: quotes and escaping

var=world

# Single quotes prevent expansion
echo 'hello $var'

# Double quotes allow expansion
echo "hello $var"

# Backslash escapes in double quotes
echo "hello \"quoted\" world"

# Backslash before dollar
echo \$var

# Mixed quotes in one word
echo "hi"' there '"$var"

# Backslash before literal
echo x\a\b\c

# Word with spaces in double quotes
msg="one two three"
echo $msg
echo "$msg"

# Escaped single quote inside a single-quoted word
echo 'it'\''s $var'

# Multiple escaped single quotes in one word
echo 'a'\''b'\''c'

# Empty single quotes between text
echo a''b
