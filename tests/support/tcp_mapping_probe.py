#!/usr/bin/env python3
"""Bounded synthetic TCP mapping checks; Python 3.9+, standard library only.

serve runs on the controlled host; client connects to an official viewer's local
mapped port. Both endpoints are fixed to 127.0.0.1. No accounts, GUI or shell access.
plan and selfcheck are offline and create no sockets.

Close-order diagnostic probe. Default immediate mode preserves the original
application protocol and close deadline. ack/delay modes control the synthetic
target, not UU. Logs contain only identifiers, counts, timing and enum/bool state.
"""
import argparse
import asyncio
import contextlib
import hashlib
import json
import math
import signal
import socket
import struct
import sys
import time
from dataclasses import dataclass, replace

MIB = 1024 * 1024
BANNER = b"UUHLV1\n"
HEADER = struct.Struct("!4sBBHIIQ")
MAGIC = b"HUT1"
ECHO, UPLOAD, DOWNLOAD, RESET = 1, 2, 3, 4
SCENARIOS = ("smoke", "large", "concurrency", "churn", "slow", "abort")
CLOSE_MODES = {"immediate": 0, "ack": 1, "delay": 2}
CLOSE_NAMES = {value: name for name, value in CLOSE_MODES.items()}
ACK = struct.Struct("!4sQ32s")
ACK_MAGIC = b"HUA1"


def emit(**values):
    values.setdefault("ts_unix_ns", time.time_ns())
    print(json.dumps(values, sort_keys=True, separators=(",", ":")), flush=True)


def content(nonce, length):
    return hashlib.shake_256(b"headless-uu-live-v1" + struct.pack("!Q", nonce)).digest(length)


def digest(data):
    return hashlib.sha256(data).digest()


def reset(writer):
    # A TCP reset is intentional in abort cases; no half-close assumption is made.
    sock = writer.get_extra_info("socket")
    if sock is not None:
        with contextlib.suppress(OSError):
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
    writer.transport.abort()


async def close(writer, timeout=1):
    started = time.monotonic()
    result = dict(close_wait_timed_out=False, close_wait_error=False, close_forced_abort=False)
    writer.close()
    try:
        await asyncio.wait_for(writer.wait_closed(), timeout)
    except asyncio.TimeoutError:
        result["close_wait_timed_out"] = True
        result["close_forced_abort"] = True
        writer.transport.abort()
    except (Exception, asyncio.CancelledError):
        result["close_wait_error"] = True
        result["close_forced_abort"] = True
        writer.transport.abort()
    result["close_wait_ms"] = round((time.monotonic() - started) * 1000, 3)
    return result


class ProtocolError(Exception):
    pass


class IntegrityError(Exception):
    pass


class BudgetError(Exception):
    pass


@dataclass(frozen=True)
class Case:
    phase: str
    op: int = ECHO
    size: int = 0
    chunk: int = 16384
    server_delay_ms: int = 0
    send_delay_ms: int = 0
    read_delay_ms: int = 0
    abort: bool = False
    connect_only: bool = False
    close_mode: int = 0


def allowance(case):
    overhead = len(BANNER) + HEADER.size
    if case.connect_only:
        return len(BANNER)
    if case.close_mode == CLOSE_MODES["ack"] and not case.abort and case.op != RESET:
        overhead += ACK.size
    if case.op == ECHO:
        return overhead + 2 * case.size
    if case.op == UPLOAD:
        # Reserve the whole declared stream even when the client will abort early.
        return overhead + case.size + 32
    return overhead + case.size


def build_cases(args):
    cases = []
    phases = SCENARIOS if args.scenario == "all" else (args.scenario,)
    for _ in range(args.rounds):
        for phase in phases:
            if phase == "smoke":
                cases += [Case(phase, size=n) for n in (0, 1, 7, 63, 1024, 65535, 65536, 192 * 1024)]
            elif phase == "large":
                cases += [Case(phase, size=n * MIB) for n in (1, 4, 8)]
            elif phase == "concurrency":
                cases += [Case(phase, size=args.concurrency_size_bytes) for _ in range(args.concurrency)]
            elif phase == "churn":
                cases += [Case(phase, connect_only=True) for _ in range(100)]
                cases += [Case("churn_recovery", size=4096)]
            elif phase == "slow":
                cases += [
                    Case("slow_client_read", DOWNLOAD, 4 * MIB, chunk=4096, read_delay_ms=5),
                    Case("slow_target_read", UPLOAD, 4 * MIB, chunk=4096, server_delay_ms=5),
                    Case("slow_client_write", UPLOAD, MIB, chunk=1024, send_delay_ms=1),
                    Case("slow_target_write", DOWNLOAD, MIB, chunk=1024, server_delay_ms=1),
                ]
            elif phase == "abort":
                cases += [Case("client_abort", UPLOAD, 2 * MIB, abort=True) for _ in range(10)]
                cases += [Case("target_abort", RESET, 4096) for _ in range(10)]
                cases += [Case("abort_recovery", size=4096) for _ in range(10)]
            elif phase == "single":
                cases += [Case(phase, op={"echo": ECHO, "upload": UPLOAD, "download": DOWNLOAD}[args.op],
                               size=args.size_bytes, chunk=args.chunk_bytes,
                               read_delay_ms=args.read_delay_ms,
                               server_delay_ms=args.server_delay_ms,
                               send_delay_ms=args.send_delay_ms)]
    return [replace(case, close_mode=CLOSE_MODES[args.close_mode]) for case in cases]


def plan(args):
    cases = build_cases(args)
    byte_limit = args.max_mib * MIB
    reserved = sum(map(allowance, cases))
    if reserved > byte_limit:
        raise BudgetError()
    counts = {}
    for case in cases:
        counts[case.phase] = counts.get(case.phase, 0) + 1
    emit(event="plan", planned_connections=len(cases), reserved_bytes=reserved,
         byte_limit=byte_limit, concurrency=args.concurrency,
         concurrency_size_bytes=args.concurrency_size_bytes,
         max_seconds=args.max_seconds, flow_seconds=args.flow_seconds,
         close_wait_seconds=args.close_wait_seconds, close_mode=args.close_mode,
         phase_counts=counts)
    return cases


async def write_data(writer, data, chunk, delay_ms, counts):
    for offset in range(0, len(data), chunk):
        piece = data[offset:offset + chunk]
        writer.write(piece)
        counts["submitted"] = counts.get("submitted", 0) + len(piece)
        await writer.drain()
        counts["sent"] += len(piece)
        if delay_ms:
            await asyncio.sleep(delay_ms / 1000)


async def read_data(reader, length, chunk, delay_ms, counts):
    result = hashlib.sha256()
    remaining = length
    while remaining:
        try:
            piece = await reader.readexactly(min(chunk, remaining))
        except asyncio.IncompleteReadError as error:
            counts["received"] += len(error.partial)
            raise
        result.update(piece)
        counts["received"] += len(piece)
        remaining -= len(piece)
        if delay_ms:
            await asyncio.sleep(delay_ms / 1000)
    return result.digest()


async def client_case(case, nonce, args):
    counts = {"sent": 0, "received": 0, "submitted": 0}
    started = time.monotonic()
    writer = None
    fingerprint = None
    outcome = "ok"
    stage = "connect"
    close_stats = {}

    async def exchange():
        nonlocal writer, fingerprint, stage
        reader, writer = await asyncio.open_connection("127.0.0.1", args.port, limit=65536)
        # No payload is sent unless the mapped target proves it is this test service.
        stage = "banner"
        if await reader.readexactly(len(BANNER)) != BANNER:
            raise ProtocolError()
        counts["received"] += len(BANNER)
        if case.connect_only:
            return
        stage = "header"
        writer.write(HEADER.pack(MAGIC, case.op, case.close_mode, case.server_delay_ms,
                                 case.size, case.chunk, nonce))
        counts["submitted"] += HEADER.size
        await writer.drain()
        counts["sent"] += HEADER.size
        if case.abort:
            stage = "client_abort"
            data = content(nonce, min(case.size, 4096))
            await write_data(writer, data, 4096, 0, counts)
            await asyncio.sleep(0.02)
            reset(writer)
            return
        if case.op == RESET:
            stage = "target_abort"
            try:
                await write_data(writer, content(nonce, case.size), case.chunk, 0, counts)
                if await reader.read(1) != b"":
                    raise ProtocolError()
            except (ConnectionResetError, BrokenPipeError):
                pass
            return
        stage = "transfer"
        expected = content(nonce, case.size)
        expected_hash = digest(expected)
        if case.op == ECHO:
            # Concurrent reading avoids a client-induced deadlock on bounded transport queues.
            send = asyncio.create_task(write_data(writer, expected, case.chunk, case.send_delay_ms, counts))
            receive = asyncio.create_task(read_data(reader, case.size, case.chunk, case.read_delay_ms, counts))
            try:
                _, actual_hash = await asyncio.gather(send, receive)
            finally:
                for task in (send, receive):
                    task.cancel()
                await asyncio.gather(send, receive, return_exceptions=True)
        elif case.op == UPLOAD:
            await write_data(writer, expected, case.chunk, case.send_delay_ms, counts)
            try:
                actual_hash = await reader.readexactly(32)
            except asyncio.IncompleteReadError as error:
                counts["received"] += len(error.partial)
                raise
            counts["received"] += 32
        elif case.op == DOWNLOAD:
            actual_hash = await read_data(reader, case.size, case.chunk, case.read_delay_ms, counts)
        else:
            raise ProtocolError()
        if actual_hash != expected_hash:
            raise IntegrityError()
        fingerprint = expected_hash.hex()
        if case.close_mode == CLOSE_MODES["ack"]:
            stage = "ack"
            acknowledgement = ACK.pack(ACK_MAGIC, nonce, actual_hash)
            writer.write(acknowledgement)
            counts["submitted"] += len(acknowledgement)
            await writer.drain()
            counts["sent"] += len(acknowledgement)
        # Verify target EOF crosses the mapping; no TCP half-close is needed by the test.
        stage = "eof"
        if await reader.read(1) != b"":
            raise ProtocolError()
        stage = "complete"

    try:
        await asyncio.wait_for(exchange(), args.flow_seconds)
    except asyncio.TimeoutError:
        outcome = "timeout"
    except IntegrityError:
        outcome = "integrity"
    except ProtocolError:
        outcome = "protocol"
    except (OSError, asyncio.IncompleteReadError):
        outcome = "connection"
    except asyncio.CancelledError:
        outcome = "cancelled"
        raise
    finally:
        if writer is not None:
            close_stats = await close(writer, args.close_wait_seconds)
        emit(event="client_flow", flow_id=nonce, phase=case.phase,
             op=0 if case.connect_only else case.op, declared_bytes=case.size,
             close_mode=CLOSE_NAMES[case.close_mode], outcome=outcome, stage=stage,
             write_submitted_bytes=counts["submitted"], drain_completed_bytes=counts["sent"],
             received_bytes=counts["received"], data_verified=fingerprint is not None,
             elapsed_ms=round((time.monotonic() - started) * 1000, 3), **close_stats)
        if outcome != "ok":
            emit(event="flow_failure", flow_id=nonce, phase=case.phase,
                 op=0 if case.connect_only else case.op, size=case.size,
                 outcome=outcome, sent_bytes=counts["sent"],
                 received_bytes=counts["received"])
    return dict(flow_id=nonce, phase=case.phase, outcome=outcome, sent=counts["sent"],
                received=counts["received"], latency_ms=round((time.monotonic() - started) * 1000, 3),
                sha256=fingerprint, deliberate_abort=case.abort or case.op == RESET,
                stage=stage, write_submitted_bytes=counts["submitted"], **close_stats)


def summary(results, event, elapsed):
    failures = {}
    latencies = sorted(item["latency_ms"] for item in results)
    verified = hashlib.sha256()
    for item in sorted(results, key=lambda entry: entry["flow_id"]):
        if item["outcome"] != "ok":
            failures[item["outcome"]] = failures.get(item["outcome"], 0) + 1
        if item["sha256"]:
            verified.update(bytes.fromhex(item["sha256"]))
    def percentile(fraction):
        return latencies[max(0, math.ceil(len(latencies) * fraction) - 1)] if latencies else 0
    emit(event=event, completed=len(results), ok=sum(item["outcome"] == "ok" for item in results),
         failures=failures, deliberate_aborts=sum(item["deliberate_abort"] for item in results),
         verified=sum(item["sha256"] is not None for item in results),
         sent_bytes=sum(item["sent"] for item in results),
         received_bytes=sum(item["received"] for item in results),
         close_wait_timeouts=sum(item.get("close_wait_timed_out", False) for item in results),
         close_forced_aborts=sum(item.get("close_forced_abort", False) for item in results),
         verified_hash_sha256=verified.hexdigest(), elapsed_seconds=round(elapsed, 3),
         latency_p50_ms=percentile(.5), latency_p95_ms=percentile(.95),
         latency_max_ms=percentile(1))


async def client(args):
    cases = plan(args)
    semaphore = asyncio.Semaphore(args.concurrency)
    results = []
    started = time.monotonic()
    nonce = 0
    tasks = []
    timed_out = False

    async def one(case, identifier):
        async with semaphore:
            result = await client_case(case, identifier, args)
            results.append(result)

    async def run_phases():
        nonlocal nonce, tasks
        offset = 0
        while offset < len(cases):
            phase = cases[offset].phase
            end = offset + 1
            while end < len(cases) and cases[end].phase == phase:
                end += 1
            before = len(results)
            phase_started = time.monotonic()
            # Churn is a rapid lifecycle check, not an unbounded connection flood.
            tasks = []
            for case in cases[offset:end]:
                nonce += 1
                tasks.append(asyncio.create_task(one(case, nonce)))
            await asyncio.gather(*tasks)
            summary(results[before:], "phase_" + phase, time.monotonic() - phase_started)
            if any(item["outcome"] != "ok" for item in results[before:]):
                return
            offset = end

    try:
        await asyncio.wait_for(run_phases(), args.max_seconds)
    except asyncio.TimeoutError:
        timed_out = True
    finally:
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
    summary(results, "client_summary", time.monotonic() - started)
    passed = not timed_out and len(results) == len(cases) and all(item["outcome"] == "ok" for item in results)
    emit(event="result", passed=passed, deadline_hit=timed_out,
         not_completed=len(cases) - len(results))
    return 0 if passed else 1


async def serve(args):
    stats = dict(accepted=0, completed=0, peer_aborts=0, target_aborts=0,
                 protocol_errors=0, internal_errors=0, timeouts=0, rejected=0, active=0,
                 peak_active=0, sent_bytes=0, received_bytes=0, reserved_bytes=0,
                 acknowledged=0, ack_timeouts=0, close_wait_timeouts=0, close_forced_aborts=0)
    tasks = set()
    writers = set()
    started = time.monotonic()
    stopping = asyncio.Event()
    loop = asyncio.get_running_loop()
    for sig in (signal.SIGINT, signal.SIGTERM):
        with contextlib.suppress(NotImplementedError):
            loop.add_signal_handler(sig, stopping.set)

    async def handle(reader, writer):
        task = asyncio.current_task()
        tasks.add(task)
        writers.add(writer)
        stats["accepted"] += 1
        connection_id = stats["accepted"]
        flow = dict(sent=0, received=0, submitted=0)
        nonce = None
        op = None
        length = 0
        flags = 0
        stage = "banner"
        outcome = "ok"
        flow_started = time.monotonic()
        overhead = len(BANNER) + HEADER.size
        if (stats["accepted"] > args.max_connections or stats["active"] >= args.max_active
                or stats["reserved_bytes"] + overhead > args.max_mib * MIB):
            stats["rejected"] += 1
            close_result = await close(writer, args.close_wait_seconds)
            stats["close_wait_timeouts"] += close_result["close_wait_timed_out"]
            stats["close_forced_aborts"] += close_result["close_forced_abort"]
            emit(event="server_flow", connection_id=connection_id, flow_id=None,
                 outcome="rejected", stage="admission", write_submitted_bytes=0,
                 drain_completed_bytes=0, received_bytes=0, **close_result)
            tasks.discard(task)
            writers.discard(writer)
            return
        stats["active"] += 1
        # Reserve header/banner even for connect-and-close and malformed requests.
        stats["reserved_bytes"] += overhead
        stats["peak_active"] = max(stats["peak_active"], stats["active"])

        async def counted_read(length):
            try:
                data = await reader.readexactly(length)
            except asyncio.IncompleteReadError as error:
                stats["received_bytes"] += len(error.partial)
                flow["received"] += len(error.partial)
                raise
            stats["received_bytes"] += len(data)
            flow["received"] += len(data)
            return data

        async def counted_write(data):
            writer.write(data)
            flow["submitted"] += len(data)
            await writer.drain()
            stats["sent_bytes"] += len(data)
            flow["sent"] += len(data)

        async def exchange():
            nonlocal nonce, op, length, flags, stage
            await counted_write(BANNER)
            stage = "header"
            raw = await counted_read(HEADER.size)
            magic, op, flags, delay_ms, length, chunk, nonce = HEADER.unpack(raw)
            if magic != MAGIC or op not in (ECHO, UPLOAD, DOWNLOAD, RESET) or flags not in CLOSE_NAMES:
                raise ProtocolError()
            if not 1 <= chunk <= 65536 or not 0 <= length <= 16 * MIB or delay_ms > 20:
                raise ProtocolError()
            reserved = allowance(Case("server", op=op, size=length, close_mode=flags)) - overhead
            if stats["reserved_bytes"] + reserved > args.max_mib * MIB:
                raise BudgetError()
            stats["reserved_bytes"] += reserved
            stage = "transfer"
            if op == RESET:
                await counted_read(min(length, 1024))
                stats["target_aborts"] += 1
                reset(writer)
                return
            if op == DOWNLOAD:
                data = content(nonce, length)
                for offset in range(0, length, chunk):
                    piece = data[offset:offset + chunk]
                    await counted_write(piece)
                    if delay_ms:
                        await asyncio.sleep(delay_ms / 1000)
            else:
                received = hashlib.sha256()
                remaining = length
                while remaining:
                    piece = await counted_read(min(chunk, remaining))
                    received.update(piece)
                    remaining -= len(piece)
                    if delay_ms:
                        await asyncio.sleep(delay_ms / 1000)
                    if op == ECHO:
                        await counted_write(piece)
                if op == UPLOAD:
                    await counted_write(received.digest())
            # Keep the baseline meaning: target work finished, not delivery confirmed.
            stats["completed"] += 1
            stage = "produced"
            if flags == CLOSE_MODES["ack"]:
                stage = "ack"
                try:
                    raw_ack = await asyncio.wait_for(counted_read(ACK.size), args.ack_seconds)
                except asyncio.TimeoutError:
                    stats["ack_timeouts"] += 1
                    raise
                ack_magic, ack_nonce, ack_hash = ACK.unpack(raw_ack)
                expected_ack = digest(data) if op == DOWNLOAD else received.digest()
                if ack_magic != ACK_MAGIC or ack_nonce != nonce or ack_hash != expected_ack:
                    raise ProtocolError()
                stats["acknowledged"] += 1
                stage = "acknowledged"
            elif flags == CLOSE_MODES["delay"]:
                stage = "close_delay"
                await asyncio.sleep(args.close_delay_ms / 1000)
            else:
                stage = "immediate_close"

        try:
            await asyncio.wait_for(exchange(), args.flow_seconds)
        except (asyncio.IncompleteReadError, ConnectionResetError, BrokenPipeError):
            outcome = "peer_abort"
            stats["peer_aborts"] += 1
        except asyncio.TimeoutError:
            outcome = "timeout"
            stats["timeouts"] += 1
        except BudgetError:
            outcome = "rejected"
            stats["rejected"] += 1
        except (ProtocolError, OSError):
            outcome = "protocol"
            stats["protocol_errors"] += 1
        except asyncio.CancelledError:
            outcome = "cancelled"
            raise
        except Exception:
            outcome = "internal"
            stats["internal_errors"] += 1
        finally:
            close_result = await close(writer, args.close_wait_seconds)
            stats["close_wait_timeouts"] += close_result["close_wait_timed_out"]
            stats["close_forced_aborts"] += close_result["close_forced_abort"]
            emit(event="server_flow", connection_id=connection_id, flow_id=nonce,
                 op=op, declared_bytes=length, close_mode=CLOSE_NAMES.get(flags, "invalid"),
                 outcome=outcome, stage=stage, intentional_reset=op == RESET,
                 write_submitted_bytes=flow["submitted"], drain_completed_bytes=flow["sent"],
                 received_bytes=flow["received"],
                 elapsed_ms=round((time.monotonic() - flow_started) * 1000, 3), **close_result)
            writers.discard(writer)
            tasks.discard(task)
            stats["active"] -= 1

    server = await asyncio.start_server(handle, "127.0.0.1", args.port, limit=65536, backlog=128)
    emit(event="server_ready", port=args.port, max_seconds=args.max_seconds,
         byte_limit=args.max_mib * MIB, max_connections=args.max_connections,
         max_active=args.max_active, flow_seconds=args.flow_seconds,
         close_wait_seconds=args.close_wait_seconds, close_delay_ms=args.close_delay_ms,
         ack_seconds=args.ack_seconds)
    try:
        while not stopping.is_set() and time.monotonic() - started < args.max_seconds:
            try:
                await asyncio.wait_for(stopping.wait(), min(5, max(.01, args.max_seconds - (time.monotonic() - started))))
            except asyncio.TimeoutError:
                emit(event="server_counts", **stats)
    finally:
        server.close()
        await server.wait_closed()
        for writer in list(writers):
            writer.transport.abort()
        pending = list(tasks)
        for task in pending:
            task.cancel()
        await asyncio.gather(*pending, return_exceptions=True)
        emit(event="server_summary", elapsed_seconds=round(time.monotonic() - started, 3), **stats)
    return 0


def bounded(low, high):
    def parse(value):
        number = int(value)
        if not low <= number <= high:
            raise argparse.ArgumentTypeError("value outside bounded range")
        return number
    return parse


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="mode", required=True)
    for mode in ("plan", "client"):
        command = sub.add_parser(mode)
        command.add_argument("--port", type=bounded(1024, 65535), default=22421)
        command.add_argument("--scenario", choices=SCENARIOS + ("all", "single"), default="all")
        command.add_argument("--concurrency", type=bounded(1, 64), default=8)
        command.add_argument("--concurrency-size-bytes", type=bounded(1, 16 * MIB), default=512 * 1024)
        command.add_argument("--rounds", type=bounded(1, 20), default=1)
        command.add_argument("--max-mib", type=bounded(1, 512), default=256)
        command.add_argument("--max-seconds", type=bounded(1, 600), default=180)
        command.add_argument("--flow-seconds", type=bounded(1, 120), default=45)
        command.add_argument("--close-mode", choices=tuple(CLOSE_MODES), default="immediate")
        command.add_argument("--close-wait-seconds", type=bounded(1, 30), default=1)
        command.add_argument("--size-bytes", type=bounded(0, 16 * MIB), default=63)
        command.add_argument("--op", choices=("echo", "upload", "download"), default="echo")
        command.add_argument("--chunk-bytes", type=bounded(1, 65536), default=16384)
        command.add_argument("--read-delay-ms", type=bounded(0, 20), default=0)
        command.add_argument("--send-delay-ms", type=bounded(0, 20), default=0)
        command.add_argument("--server-delay-ms", type=bounded(0, 20), default=0)
    server = sub.add_parser("serve")
    server.add_argument("--port", type=bounded(1024, 65535), default=42821)
    server.add_argument("--max-seconds", type=bounded(1, 1800), default=900)
    server.add_argument("--flow-seconds", type=bounded(1, 120), default=60)
    server.add_argument("--max-mib", type=bounded(1, 1024), default=256)
    server.add_argument("--max-connections", type=bounded(1, 10000), default=2000)
    server.add_argument("--max-active", type=bounded(1, 128), default=128)
    server.add_argument("--close-wait-seconds", type=bounded(1, 30), default=1)
    server.add_argument("--close-delay-ms", type=bounded(1, 5000), default=250)
    server.add_argument("--ack-seconds", type=bounded(1, 120), default=30)
    sub.add_parser("selfcheck")
    args = parser.parse_args()
    try:
        if args.mode == "plan":
            plan(args)
            return 0
        if args.mode == "selfcheck":
            # Pure codec/generator/budget checks. This command creates no network sockets.
            raw = HEADER.pack(MAGIC, ECHO, 0, 5, 192 * 1024, 4096, 1)
            assert HEADER.unpack(raw) == (MAGIC, ECHO, 0, 5, 192 * 1024, 4096, 1)
            assert content(1, 4096)[:1024] == content(1, 1024)
            assert digest(content(1, 4096)) != digest(content(2, 4096))
            assert allowance(Case("check", ECHO, 100)) == len(BANNER) + HEADER.size + 200
            token = ACK.pack(ACK_MAGIC, 7, digest(content(7, 4096)))
            assert ACK.unpack(token) == (ACK_MAGIC, 7, digest(content(7, 4096)))
            assert allowance(Case("check", ECHO, 100, close_mode=1)) == len(BANNER) + HEADER.size + 200 + ACK.size
            assert allowance(Case("check", ECHO, 100, close_mode=2)) == allowance(Case("check", ECHO, 100))
            concurrency_args = argparse.Namespace(scenario="concurrency", rounds=1,
                                                  concurrency=64, concurrency_size_bytes=512 * 1024,
                                                  close_mode="immediate")
            cases = build_cases(concurrency_args)
            assert (len(cases), {case.size for case in cases}, sum(map(allowance, cases))) == (64, {512 * 1024}, 67_110_848)
            concurrency_args.concurrency_size_bytes = 64 * 1024
            concurrency_args.close_mode = "ack"
            cases = build_cases(concurrency_args)
            assert (len(cases), {case.size for case in cases}, sum(map(allowance, cases))) == (64, {64 * 1024}, 8_393_408)
            class FakeTransport:
                aborted = False
                def abort(self): self.aborted = True
            class FakeWriter:
                def __init__(self, hang): self.transport = FakeTransport(); self.hang = hang
                def close(self): pass
                async def wait_closed(self):
                    if self.hang: await asyncio.Event().wait()
            async def check_close():
                fast = FakeWriter(False)
                done = await close(fast, .01)
                assert not done["close_wait_timed_out"] and not fast.transport.aborted
                hung = FakeWriter(True)
                timed = await close(hung, .001)
                assert timed["close_wait_timed_out"] and timed["close_forced_abort"] and hung.transport.aborted
            asyncio.run(check_close())
            emit(event="selfcheck", checks=11, passed=True, protocol_header_bytes=HEADER.size,
                 application_ack_bytes=ACK.size, network_executed=False)
            return 0
        return asyncio.run(serve(args) if args.mode == "serve" else client(args))
    except BudgetError:
        emit(event="error", category="budget_exceeded")
        return 2
    except KeyboardInterrupt:
        emit(event="error", category="interrupted")
        return 130
    except Exception:
        # Never print exception text, endpoint addresses, payloads or stack local values.
        emit(event="error", category="setup_or_runtime")
        return 1


if __name__ == "__main__":
    sys.exit(main())
