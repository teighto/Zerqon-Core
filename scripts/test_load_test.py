import asyncio
import contextlib
import importlib.util
import io
from pathlib import Path
import struct
import tempfile
import unittest
import zlib

spec = importlib.util.spec_from_file_location('load_test', Path(__file__).with_name('load-test.py'))
load = importlib.util.module_from_spec(spec)
spec.loader.exec_module(load)


def flat_chunk(x=0, z=0):
    sections = bytearray()
    for section in range(24):
        state = 1 if section < 8 else 0
        sections += struct.pack('>HH', 4096 if state else 0, 0)
        sections += b'\x00' + load.varint(state) + b'\x00\x01'
    return struct.pack('>ii', x, z) + b'\x00' + load.varint(len(sections)) + sections


class CodecTests(unittest.TestCase):
    def test_signed_varints_and_negative_block_coordinates(self):
        for number in [0, 127, 128, 777, -1, -(2**31), 2**31 - 1]:
            self.assertEqual(load.Cursor(load.varint(number)).varint(), number)
        for position in [(-17, -64, -33), (29_999_999, 319, -29_999_999)]:
            self.assertEqual(load.unpack_position(load.pack_position(position)), position)
        for bad in [b'\x80', b'\xff\xff\xff\xff\x7f', b'\x80' * 6]:
            with self.assertRaises(ValueError):
                load.Cursor(bad).varint()

    def test_compression_is_bounded_and_rejects_trailing_data(self):
        for threshold in [None, 0, 256]:
            for packet in [b'\x03', b'\x07' + b'x' * 1024]:
                wire = load.encode_frame(packet, threshold)
                view = load.Cursor(wire)
                body = view.take(view.varint())
                self.assertEqual(load.decode_frame(body, threshold), packet)
        for body in [load.varint(2_097_152), b'\x00' + b'x' * 256,
                     load.varint(256) + zlib.compress(b'x' * 256) + b'junk',
                     load.varint(256) + zlib.compress(b'x' * 10000)]:
            with self.assertRaises(ValueError):
                load.decode_frame(body, 256)

    def test_chunk_columns_use_26_3_implicit_packed_array_lengths(self):
        columns = load.chunk_columns(flat_chunk(-1, 0), [(-3, 2), (1, 2)])
        self.assertEqual(set(columns), {(-3, 2)})
        self.assertEqual(columns[(-3, 2)][127:130], [1, 0, 0])
        self.assertEqual(load.safe_floor(columns[(-3, 2)]), 64)
        with self.assertRaises(ValueError):
            load.chunk_columns(flat_chunk()[:-1], [(0, 0)])

    def test_indirect_and_global_palettes_unpack_without_word_straddling(self):
        encoded = b'\x04\x02\x00\x01' + struct.pack('>Q', 0x1010101010101010)
        self.assertEqual(load.palette_values(load.Cursor(encoded), 16, 8), [0, 1] * 8)
        encoded = b'\x09' + struct.pack('>QQ', sum(i << (9*i) for i in range(7)), 7)
        self.assertEqual(load.palette_values(load.Cursor(encoded), 8, 8), list(range(8)))

    def test_limits_and_loopback_only(self):
        self.assertEqual(load.parse_args([]).clients, 16)
        self.assertEqual(load.parse_args([]).duration, 60)
        for arguments in [['--clients', '65'], ['--duration', '601'], ['--move-hz', '21'],
                          ['--host', 'example.com'], ['--host', '192.168.1.1'],
                          ['--host', '0.0.0.0'], ['--duration', 'nan']]:
            with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                load.parse_args(arguments)

    def test_lanes_keep_build_targets_outside_every_client_body(self):
        for index in range(64):
            anchor, target = load.lane(index, (0.5, 64, 0.5))
            self.assertLess(abs(anchor[0]), 17)
            self.assertLess(abs(anchor[1]), 17)
            for other in range(64):
                body, _ = load.lane(other, (0.5, 64, 0.5))
                self.assertFalse(abs(body[0] + .5 - (target[0] + .5)) < 1.1
                                 and abs(body[1] + .5 - (target[1] + .5)) < 1.1)


class WireTests(unittest.IsolatedAsyncioTestCase):
    async def test_invalid_packet_ids_cannot_grow_metric_keys(self):
        for phase, invalid in [('login', 6), ('configuration', 21), ('play', 144), ('play', -1)]:
            client = load.Client(load.parse_args(['--duration', '1']), 0, {'clients': []})
            client.phase = phase
            client.reader = asyncio.StreamReader()
            client.reader.feed_data(load.encode_frame(load.varint(invalid), None))
            with self.assertRaises(ValueError):
                await client.read()
            self.assertEqual(client.metrics.received, {})

    async def test_fragmented_frame_and_eof_account_for_actual_bytes(self):
        reader = asyncio.StreamReader()
        packet = b'\x7c' + b'A' * 2000
        wire = load.encode_frame(packet, 256)
        async def feed():
            for byte in wire:
                reader.feed_data(bytes([byte]))
                await asyncio.sleep(0)
        metrics = load.Metrics()
        task = asyncio.create_task(feed())
        self.assertEqual(await load.read_frame(reader, 256, metrics), packet)
        await task
        self.assertEqual(metrics.wire_rx, len(wire))
        reader.feed_data(b'\x03x')
        reader.feed_eof()
        with self.assertRaises(asyncio.IncompleteReadError):
            await load.read_frame(reader, None, metrics)
        self.assertEqual(metrics.wire_rx, len(wire) + 2)

    async def test_ack_without_matching_update_is_not_a_success(self):
        client = load.Client(load.parse_args(['--duration', '1']), 0, {'clients': []})
        client.pending_edit = {'sequence': 1, 'position': (2, 64, 0), 'expected': 1,
                               'seen': False, 'rejection': None, 'sent': 0,
                               'future': asyncio.get_running_loop().create_future()}
        await client.handle_play(4, b'\x01')
        self.assertFalse(client.pending_edit['future'].result()['ok'])

    async def test_chunk_unload_removes_permission_to_move_or_edit_there(self):
        client = load.Client(load.parse_args(['--duration', '1']), 0, {'clients': []})
        client.loaded.add((-2, 3))
        await client.handle_play(38, struct.pack('>ii', 3, -2))
        self.assertNotIn((-2, 3), client.loaded)
        with self.assertRaises(ValueError):
            await client.move((-31.5, 64.0, 48.5))

    async def test_truncated_play_packet_records_disconnect_instead_of_losing_reader(self):
        client = load.Client(load.parse_args(['--duration', '1']), 0, {'clients': []})
        client.phase = 'play'
        client.reader = asyncio.StreamReader()
        client.current_batch = 0
        client.reader.feed_data(load.encode_frame(b'\x2e\x00', None))
        client.reader.feed_eof()
        await client.receive()
        self.assertIn('truncated packet', client.disconnect)
        self.assertTrue(client.stop.is_set())

    async def test_login_rejection_preserves_actual_reason_and_closes_connection(self):
        closed = asyncio.Event()
        failures = []
        async def server(reader, writer):
            try:
                metrics = load.Metrics()
                await load.read_frame(reader, None, metrics)
                await load.read_frame(reader, None, metrics)
                text = b'Java-compatible client connections are disabled on this server.'
                writer.write(load.encode_frame(b'\x00' + load.string('{"text":"' + text.decode() + '"}'), None))
                await writer.drain()
                await asyncio.wait_for(reader.read(), 3)
            except Exception as error:
                failures.append(error)
            finally:
                writer.close()
                await writer.wait_closed()
                closed.set()
        listener = await asyncio.start_server(server, '127.0.0.1', 0)
        try:
            with tempfile.TemporaryDirectory() as directory:
                args = load.parse_args(['--clients', '1', '--duration', '1', '--port', str(listener.sockets[0].getsockname()[1]), '--output', str(Path(directory) / 'report.json')])
                report = await load.run(args)
                self.assertEqual(report['joined'], 0)
                self.assertEqual(report['failed_clients'], 1)
                self.assertIn('Java-compatible client connections are disabled', report['clients'][0]['join_or_workload_failure'])
                self.assertTrue(report['all_connections_closed'])
                await asyncio.wait_for(closed.wait(), 3)
                self.assertEqual(failures, [])
        finally:
            listener.close()
            await listener.wait_closed()

    async def test_missing_empty_and_mismatched_initial_batches_do_not_complete_join(self):
        for actual, declared in [(None, 1), (0, 0), (1, 2)]:
            client = load.Client(load.parse_args(['--duration', '1']), 0, {'clients': []})
            client.current_batch = actual
            with self.assertRaises(ValueError):
                await client.handle_play(11, load.varint(declared))
            self.assertEqual(client.batches, 0)

    async def test_close_timeout_aborts_transport_and_is_not_reported_as_clean_close(self):
        class Writer:
            def __init__(self):
                self.transport = self
                self.aborted = False
            def close(self):
                pass
            async def wait_closed(self):
                raise asyncio.TimeoutError()
            def abort(self):
                self.aborted = True
        client = load.Client(load.parse_args(['--duration', '1']), 0, {'clients': []})
        client.writer = Writer()
        await client.close()
        await client.close()
        self.assertTrue(client.writer.aborted)
        self.assertFalse(client.closed)
        self.assertEqual(client.close_error, 'TimeoutError')


if __name__ == '__main__':
    unittest.main()
