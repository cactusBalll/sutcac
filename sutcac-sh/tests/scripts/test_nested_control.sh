#!/usr/bin/env bash
# Test: deeply nested control flow with break/continue

# Find first pair where product is divisible by 6, break out of both loops
for ((i = 1; i <= 4; i++)); do
    for ((j = 1; j <= 4; j++)); do
        if ((i * j % 6 == 0)); then
            echo "break2: i=$i j=$j"
            break 2
        fi
    done
done

# Skip inner iterations but keep outer going
outer=0
for ((i = 1; i <= 3; i++)); do
    for ((j = 1; j <= 3; j++)); do
        if ((j == 2)); then
            continue
        fi
        echo "inner-skip: i=$i j=$j"
    done
done

# continue 2: skip to next outer iteration
for ((i = 1; i <= 3; i++)); do
    for ((j = 1; j <= 3; j++)); do
        if ((i == 2)); then
            continue 2
        fi
        echo "continue2: i=$i j=$j"
    done
    echo "outer-end i=$i"
done

# FizzBuzz using arithmetic for and if/!(( ))
for ((n = 1; n <= 15; n++)); do
    if ((n % 15 == 0)); then
        echo "FizzBuzz"
    elif ((n % 3 == 0)); then
        echo "Fizz"
    elif ((n % 5 == 0)); then
        echo "Buzz"
    else
        echo "$n"
    fi
done

# Sum 1..10 with arithmetic for
sum=0
for ((k = 1; k <= 10; k++)); do
    ((sum += k))
done
echo "sum=$sum"

# Factorial 5!
fact=1
for ((m = 1; m <= 5; m++)); do
    ((fact *= m))
done
echo "5!=$fact"
