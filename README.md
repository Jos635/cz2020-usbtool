# cz2020-usbtool

A tool for communicating with the CampZone 2020 badge.

# Usage

```
cz2020-usbtool 0.1.0
Communicate with the CampZone 2020 badge without using Chrome.

USAGE:
    cz2020-usbtool <SUBCOMMAND>

FLAGS:
    -h, --help       Prints help information
    -V, --version    Prints version information

SUBCOMMANDS:
    cp             Copies a file to another file
    create-dir     Creates a new directory
    create-file    Creates a new file
    get            Fetches the specified file
    help           Prints this message or the help of the given subcommand(s)
    ls             Lists all files in the specified directory
    mount          Mounts the filesystem of the badge to a directory using libfuse
    mv             Moves a file from one location to another
    rm             Deletes the specified path
    run            Runs an app
    set            Writes stdin to the specified file
    shell          Opens the serial connection for the Python shell on the badge. Input from standard in is written
                   to the device.
    tree           Lists all files available on the badge one-by-one
```

## Mounting
You can mount the badge's filesystem using the `mount` verb:

```
mkdir cz2020
./cz2020-usbtool mount cz2020
```

To safely unmount, use umount:
```
umount cz2020
```

If you happen to like using `^C` on everything you see, or if the tool crashed and you see a "Transport endpoint is not connected" error, you might need to do:

```
sudo umount cz2020
```

If you mount the filesystem, you won't be able to run a second instance of the tool to execute another command. In order to run files and use the Python shell, two special files are mounted: `run` and `serial`. You can write a path to `run` to run that file. For example, `echo /apps/synthesizer/__init__.py > run` will run the synthesizer. You can use the `serial` file to read and write to the Python shell running on the device. For example, using minicom: `minicom --device serial`.

**Note**: Enumerating directory entries can be quite slow, because we need to fetch the entire file to determine its size. For example, if you run `ls /flash/cache/system` the tool needs to download all mp3 files in that directory. This can take a while.