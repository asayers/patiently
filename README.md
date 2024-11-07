<h1 align="center">Patiently</h1>
<p align="center">A simple job queue with parallelism</p>

Running `patiently $cmd` is exactly the same as running `$cmd`, except that
it will wait its turn before running.  This lets you start all your jobs in
the background, without causing your CPU to explode.

```console
$ # Sort all txt files in the working dir, using up to 8 cores at once
$ for x in *.txt; do patiently -j8 sort $x > $x_sorted.txt & done
$ patiently
   waiting: 632
   running: 8
  finished: 17
    failed: 0
   crashed: 0
```

## What does it do?

If you run this:

```console
$ seq 10000 & seq 10000 & seq 10000 & seq 10000 &
```

...then all four jobs will start running immediately, and you'll see a bunch of
interleaved numbers:

```console
[...]
9998
9998
9999
9999
9999
10000
10000
10000
[4]  + done       seq 10000
[2]    done       seq 10000
[3]  + done       seq 10000
[1]    done       seq 10000
```

But if you run it with `patiently`...

```console
$ patiently seq 10000 & patiently seq 10000 & patiently seq 10000 & patiently seq 10000 &
```

...you'll see the same numbers, but the output is no longer interleaved:

```console
[...]
9996
9997
9998
9999
10000
[4]  + done       patiently seq 10000
```

What happened?

* When the first job started it created a job queue (in `$PWD/.patiently`) and then ran `seq`.
* When the second, third, and fourth jobs started, they saw that the first job was already running, and went to sleep.
* When the first job finished, the second job woke up and ran `seq`.
* etc.

## How do you use it?

Typically you'd spawn your jobs using a loop, and then wait for them all to
finish; eg.:

```bash
for x in $inputs; do
    patiently myprog $x | grep ERROR >$x.errors &
done
wait
```

If you want to run a bounded number of jobs in parallel, you can pass the `-j`
flag. For example, to sort all .txt files in the working dir, using up to 8
cores at once:

```bash
for x in *.txt; do
    patiently -j8 sort $x > $x_sorted.txt &
done
wait
```

Instead of calling `wait` at the end, you can alternatively run `patiently`
without any arguments to see a live status report:

```console
$ patiently
   waiting: 632
   running: 8
  finished: 17
    failed: 0
   crashed: 0
```

## Inspiration

The design of patiently is completely inspired by [nq].  The main differences are:

* nq doesn't support parallelism (ie. no `-j` flag).
* nq redirects stdout/stderr to a file automatically, patiently passes
  them through to the terminal (unless you redirect them yourself).
* nq forks automatically, patiently remains in the foreground (unless you
  background it yourself).

[nq]: https://github.com/leahneukirchen/nq
