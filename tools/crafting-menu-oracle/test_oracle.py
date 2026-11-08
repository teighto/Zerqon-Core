"""Checks the real-server oracle boundary; no Minecraft menu semantics are mocked."""
import argparse
import copy
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
import unittest

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
spec = importlib.util.spec_from_file_location('menu_runner',HERE/'run.py')
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)
JAVA = 'java'
JAVAC = 'javac'
REPORT = None


class MenuOracleTests(unittest.TestCase):
    def test_read_deadline_is_hard_before_any_socket_access(self):
        client = object.__new__(runner.Client)
        with self.assertRaises(TimeoutError):
            client.read(-1)
        with self.assertRaises(TimeoutError):
            client.exact(1,time.monotonic()-1)

    def test_drop_aggregation_preserves_quantity_across_entity_merging(self):
        lines = ['[server] Glass Bottle has the following entity data: {count: 1, id: "minecraft:glass_bottle"}',
                 '[server] Glass Bottle has the following entity data: {count: 3, id: "minecraft:glass_bottle"}']
        self.assertEqual(runner.drops(lines),[{'item':'minecraft:glass_bottle','count':4,'components':{}}])
        with self.assertRaises(ValueError):
            runner.drops(['[server] Unexpected command error'])

    def test_default_stack_reader_rejects_unhandled_components(self):
        client = object.__new__(runner.Client)
        client.names = {1:'minecraft:stone'}
        with self.assertRaisesRegex(ValueError,'Unexpected component patch'):
            client.stack(runner.wire.Cursor(bytes([1,1,1,0])))

    def test_abnormal_owned_process_exit_is_preserved(self):
        server = object.__new__(runner.Server)
        server.process = subprocess.Popen([sys.executable,'-c','raise SystemExit(7)'],stdin=subprocess.PIPE,stdout=subprocess.PIPE,text=True)
        server.thread = threading.Thread(target=lambda:server.process.stdout.read(),daemon=True)
        server.thread.start()
        server.process.wait(timeout=5)
        cleanup = server.stop()
        self.assertEqual(cleanup,{'server_exit_code':7,'forced_kill':False,'log_reader_stopped':True})
        self.assertTrue(server.process.stdin.closed)
        self.assertTrue(server.process.stdout.closed)

    def test_each_real_after_snapshot_replays_at_its_wire_ack(self):
        report = json.loads(REPORT.read_text(encoding='utf-8'))
        self.assertEqual(report['status'],'passed')
        self.assertEqual(report['cleanup'],{'server_exit_code':0,'forced_kill':False,'log_reader_stopped':True})
        items = json.loads((REPORT.parent/'items.json').read_text(encoding='utf-8'))['items']
        self.assertGreaterEqual(len(report['cases']),25)
        for case in report['cases']:
            with self.subTest(case=case['name']):
                client = object.__new__(runner.Client)
                client.names = {entry['id']:name for name,entry in items.items()}
                client.slots,client.cursor,client.selected = [None]*46,None,0
                client.full,client.state,client.game_mode,client.trace = 0,0,1,[]
                points = {value['wire_end']:(index,value) for index,value in enumerate(case['wire_steps'])}
                for position,event in enumerate(case['wire'],1):
                    if event['direction'] == 'serverbound' and event['id'] == 54:
                        client.selected = runner.struct.unpack('>h',bytes.fromhex(event['body_hex']))[0]
                    if event['direction'] == 'clientbound':
                        client.handle(event['id'],bytes.fromhex(event['body_hex']))
                    if position in points:
                        index,point = points[position]
                        self.assertEqual((event['direction'],event['id']),('clientbound',4))
                        self.assertEqual(client.state,point['state_id'])
                        actual = {'slots':client.slots,'cursor':client.cursor,'selected':client.selected}
                        self.assertEqual(actual,case['after'][index])
                self.assertEqual(len(case['wire']),case['wire_steps'][-1]['wire_end'],
                                 'A later case cleanup polluted this trace')

    def test_wrong_expected_result_fails_after_real_runtime_and_reaps_server(self):
        cache = ROOT/'.cache/crafting-menu-oracle'
        cache.mkdir(parents=True,exist_ok=True)
        with tempfile.TemporaryDirectory(prefix='negative-',dir=cache) as temporary:
            stage = Path(temporary)
            fixture = json.loads((HERE/'fixtures.json').read_text(encoding='utf-8'))
            case = copy.deepcopy(next(value for value in fixture['cases'] if value['name']=='left_result'))
            case['expected_after'][0]['cursor']['count'] = 5
            fixture['cases'] = [case]
            path = stage/'wrong.json'
            path.write_text(json.dumps(fixture),encoding='utf-8')
            output = stage/'must-not-publish.json'
            result = subprocess.run([sys.executable,str(HERE/'run.py'),'--accept-eula','--java',JAVA,'--javac',JAVAC,
                                     '--fixtures',str(path),'--output',str(output)],cwd=ROOT,capture_output=True,text=True,
                                    encoding='utf-8',errors='replace',timeout=120)
            self.assertNotEqual(result.returncode,0)
            self.assertIn('Menu observation differs',result.stdout+result.stderr)
            self.assertFalse(output.exists())
            evidence = next(line.removeprefix('Evidence: ') for line in result.stdout.splitlines() if line.startswith('Evidence: '))
            report = json.loads((Path(evidence)/'report.json').read_text(encoding='utf-8'))
            self.assertEqual(report['status'],'failed')
            self.assertEqual(report['cleanup'],{'server_exit_code':0,'forced_kill':False,'log_reader_stopped':True})


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--java',default='java')
    parser.add_argument('--javac',default='javac')
    parser.add_argument('--report',type=Path,required=True,help='report.json from a successful real run')
    options,remaining = parser.parse_known_args()
    JAVA,JAVAC,REPORT = options.java,options.javac,options.report.resolve()
    unittest.main(argv=[__file__,*remaining])
