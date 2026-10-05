#!/usr/bin/env python3
"""Deny-and-log HTTP proxy for the isolation test.

Any HTTPS request routed through it (via HTTPS_PROXY) is logged (hostname
visible in the CONNECT line) and immediately refused with 403, so the client
fails fast instead of stalling on timeouts.
"""
import asyncio


async def handle(reader, writer):
    try:
        line = await asyncio.wait_for(reader.readline(), timeout=5)
        if not line:
            return
        print("PROXY-REQ:", line.decode(errors="replace").strip(), flush=True)
        # drain the remaining headers
        while True:
            h = await asyncio.wait_for(reader.readline(), timeout=5)
            if h in (b"\r\n", b"\n", b""):
                break
        writer.write(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
        await writer.drain()
    except Exception as e:
        print("PROXY-ERR:", type(e).__name__, str(e)[:120], flush=True)
    finally:
        try:
            writer.close()
        except Exception:
            pass


async def main():
    server = await asyncio.start_server(handle, "127.0.0.1", 8890)
    print("deny-proxy listening on 127.0.0.1:8890", flush=True)
    async with server:
        await server.serve_forever()


asyncio.run(main())
