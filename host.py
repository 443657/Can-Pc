#!/usr/bin/env python3
"""Exercise the DM-MC02 control port and raw UART bridge.

The control port is newline-delimited ASCII. Received Classical CAN frames are
streamed automatically as::

    RX <id> <dlc> <hex-data>

and every host command receives one ``OK ...`` or ``ERR ...`` line. See
EXTENSION.md for the complete protocol.
"""

import argparse
import time


def build_parser():
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("port", help="USB serial device name (for example /dev/ttyACM0)")
    commands = parser.add_subparsers(dest="command", required=True)

    commands.add_parser("ping", help="check the control protocol")
    commands.add_parser("swd", help="read the target DPIDR")
    commands.add_parser("stat", help="show bridge counters")
    commands.add_parser("uart-loopback", help="test the raw UART bridge")

    send = commands.add_parser("can-send", help="queue one Classical CAN frame")
    send.add_argument("id", help="11-bit hex ID, without 0x")
    send.add_argument("data", help="0-8 bytes in hex, or '-' for no data")

    monitor = commands.add_parser("can-monitor", help="print streamed CAN frames")
    monitor.add_argument("--seconds", type=float, default=5.0)
    monitor.add_argument("--count", type=int, default=None)

    recv = commands.add_parser("can-recv", help="wait for one streamed CAN frame")
    recv.add_argument("--seconds", type=float, default=5.0)

    test = commands.add_parser(
        "bridge-test", help="check line framing without a second CAN node"
    )
    test.add_argument("--seconds", type=float, default=2.0)

    return parser


def parse_id(text):
    if not text or len(text) > 3 or any(
        char not in "0123456789abcdefABCDEF" for char in text
    ):
        raise ValueError("ID must be 1-3 hex digits")
    value = int(text, 16)
    if value > 0x7FF:
        raise ValueError("standard CAN ID must be <= 7FF")
    return value


def parse_payload(text):
    if text == "-":
        return 0, "-"
    cleaned = text.replace(" ", "")
    if not cleaned or len(cleaned) % 2 or len(cleaned) > 16:
        raise ValueError("data must be 0-8 bytes of hex, or '-'")
    if any(char not in "0123456789abcdefABCDEF" for char in cleaned):
        raise ValueError("data contains a non-hex character")
    return len(cleaned) // 2, cleaned.upper()


def read_line(port, deadline):
    """Read one newline-terminated line, or return None at the deadline."""
    buffer = bytearray()
    while time.monotonic() < deadline:
        byte = port.read(1)
        if not byte:
            continue
        if byte == b"\n":
            return buffer.decode("ascii", errors="replace").rstrip("\r")
        buffer.extend(byte)
        if len(buffer) > 256:
            # The firmware caps host lines at 96 bytes. Do not grow without
            # bound if a port is misconfigured.
            buffer.clear()
    return None


def read_reply(port, timeout=2.0):
    """Return the next command reply, printing any interleaved RX events."""
    deadline = time.monotonic() + timeout
    while True:
        line = read_line(port, deadline)
        if line is None:
            return None
        if not line:
            continue
        if line.startswith("RX "):
            print(f"  {line}")
            continue
        return line


def expect(port, command, expected, timeout=2.0):
    port.write((command + "\n").encode("ascii"))
    return expect_reply(port, expected, timeout)


def expect_reply(port, expected, timeout=2.0):
    reply = read_reply(port, timeout)
    if reply is None:
        raise RuntimeError(f"no reply, expected {expected!r}")
    print(reply)
    if not reply.startswith(expected):
        raise RuntimeError(f"expected {expected!r}, received {reply!r}")
    return reply


def read_exact(port, count, timeout):
    buffer = bytearray()
    deadline = time.monotonic() + timeout
    while len(buffer) < count and time.monotonic() < deadline:
        chunk = port.read(count - len(buffer))
        if chunk:
            buffer.extend(chunk)
    return bytes(buffer)


def monitor(port, seconds, count=None):
    deadline = time.monotonic() + seconds
    seen = 0
    while time.monotonic() < deadline and (count is None or seen < count):
        window = min(deadline, time.monotonic() + 0.5)
        line = read_line(port, window)
        if line is None:
            continue
        if line.startswith("RX "):
            print(line)
            seen += 1
        elif line:
            print(f"# {line}")
    if count is not None and seen < count:
        raise RuntimeError(f"received {seen}/{count} CAN frames before timeout")
    return seen


def run(port, args):
    if args.command == "uart-loopback":
        expected = b"HerKules UART\n"
        port.write(expected)
        got = read_exact(port, len(expected), 2.0)
        if got != expected:
            raise RuntimeError(f"expected {expected!r}, received {got!r}")
        print("OK UART loopback")
        return

    if args.command == "can-send":
        frame_id = parse_id(args.id)
        dlc, data = parse_payload(args.data)
        expect(port, f"TX {frame_id:03X} {dlc} {data}", "OK TX")
        return

    if args.command == "can-monitor":
        monitor(port, args.seconds, args.count)
        return

    if args.command == "can-recv":
        monitor(port, args.seconds, 1)
        return

    if args.command == "bridge-test":
        # One request split over two USB writes: the firmware must keep the
        # partial line until the newline arrives.
        port.write(b"TX 12")
        time.sleep(0.05)
        port.write(b"3 2 AABB\n")
        expect_reply(port, "OK TX", args.seconds)

        # Two requests in one USB write: both must execute, in order.
        port.write(b"PING\nSTAT\n")
        expect_reply(port, "OK DM-MC02", args.seconds)
        expect_reply(port, "OK rx=", args.seconds)

        # An oversized line is discarded up to its newline, not executed.
        port.write(b"X" * 200 + b"\n")
        expect_reply(port, "ERR SYNTAX", args.seconds)

        # Out-of-range standard ID and DLC are rejected before the CAN queue.
        expect(port, "TX 800 1 00", "ERR FRAME", args.seconds)
        expect(port, "TX 123 9 -", "ERR FRAME", args.seconds)
        print("OK bridge framing (split, combined, oversized, invalid)")
        return

    if args.command == "ping":
        expect(port, "PING", "OK DM-MC02")
        return

    if args.command == "swd":
        expect(port, "SWD ID", "OK DPIDR ")
        return

    if args.command == "stat":
        expect(port, "STAT", "OK rx=")
        return

    raise RuntimeError(f"unknown command {args.command!r}")


def main():
    parser = build_parser()
    args = parser.parse_args()

    try:
        import serial
    except ImportError:
        parser.exit(2, "Install pyserial 3.5 in a project environment first.\n")

    try:
        with serial.Serial(args.port, 115200, timeout=0.1, write_timeout=2) as port:
            port.reset_input_buffer()
            run(port, args)
    except serial.SerialException as error:
        parser.exit(1, f"Serial error: {error}\n")
    except (RuntimeError, ValueError) as error:
        parser.exit(1, f"{error}\n")


if __name__ == "__main__":
    main()
