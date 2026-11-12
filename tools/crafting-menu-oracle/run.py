import argparse
import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import queue
import re
from collections import Counter
import socket
import struct
import subprocess
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parents[2]
HERE = Path(__file__).resolve().parent
MAX_JSON = 1_048_576
MAX_REPORT = 16_777_216

def module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value

wire = module('crafting_wire', ROOT / 'scripts/load-test.py')
facts = module('crafting_facts', ROOT / 'tools/crafting-data/generate.py')

class Client:
    def __init__(self, port, ids):
        self.sock = socket.create_connection(('127.0.0.1', port), timeout=10)
        self.port, self.ids = port, ids
        self.names = {value: key for key, value in ids.items()}
        self.threshold = None
        self.phase = 'login'
        self.slots = [None] * 46
        self.cursor = None
        self.state = 0
        self.full = 0
        self.loaded = False
        self.position = False
        self.batch = False
        self.trace = []
        self.selected = 0
        self.game_mode = 1
        self.barriers = True
        self.barrier_counter = 0

    def send(self, kind, body=b''):
        if self.phase == 'play' and kind in (18, 19, 41, 54, 57):
            self.trace.append({'direction':'serverbound','id':kind,'body_hex':body.hex()})
        self.sock.sendall(wire.encode_frame(wire.varint(kind) + body, self.threshold))

    def exact(self, size, deadline):
        result = bytearray()
        while len(result) < size:
            remaining = deadline-time.monotonic()
            if remaining <= 0:
                raise TimeoutError('Packet read deadline expired')
            self.sock.settimeout(remaining)
            chunk = self.sock.recv(size-len(result))
            if not chunk:
                raise RuntimeError('Vanilla closed client socket')
            result.extend(chunk)
        return bytes(result)

    def read(self, timeout=15):
        if timeout <= 0:
            raise TimeoutError('Protocol phase deadline expired')
        deadline = time.monotonic()+timeout
        length = 0
        for index in range(3):
            byte = self.exact(1, deadline)[0]
            length |= (byte & 127) << (7*index)
            if not byte & 128:
                break
        else:
            raise ValueError('Oversized frame length')
        if not 0 < length <= wire.MAX_FRAME:
            raise ValueError('Frame out of bounds')
        view = wire.Cursor(wire.decode_frame(self.exact(length, deadline), self.threshold))
        return view.varint(), view.rest()

    def stack(self, view):
        count = view.varint()
        if count == 0:
            return None
        item = self.names[view.varint()]
        added, removed = view.varint(), view.varint()
        if added or removed:
            raise ValueError(f'Unexpected component patch in menu fixture: {item}, {added}/{removed}')
        return {'item':item,'count':count,'components':{}}

    def handle(self, kind, body):
        if kind in (4,18,20,39,98,107,110):
            self.trace.append({'direction':'clientbound','id':kind,'body_hex':body.hex()})
        view = wire.Cursor(body)
        if kind == 32:
            raise RuntimeError('Play disconnect: '+body.hex())
        if kind == 45:
            self.send(28, body)
        elif kind == 62:
            self.send(45, body)
        elif kind == 73:
            teleport = view.varint()
            xyz = view.take(24)
            view.take(24)
            rotation = view.take(8)
            flags = view.take(4)
            if flags != bytes(4):
                raise RuntimeError('Unexpected relative teleport')
            self.send(0, wire.varint(teleport)+xyz+rotation)
            self.position = True
        elif kind == 50:
            self.loaded = True
        elif kind == 11:
            self.send(11, struct.pack('>f',8.0))
            self.batch = True
        elif kind == 18:
            container, state, size = view.varint(), view.varint(), view.varint()
            if container != 0 or size != 46:
                raise RuntimeError(f'Unexpected menu {container}/{size}')
            self.state = state
            self.slots = [self.stack(view) for _ in range(size)]
            self.cursor = self.stack(view)
            if view.rest():
                raise RuntimeError('Trailing container content bytes')
            self.full += 1
        elif kind == 20:
            container, state = view.varint(), view.varint()
            slot = struct.unpack('>h',view.take(2))[0]
            stack = self.stack(view)
            if container == 0:
                self.state = state
                self.slots[slot] = stack
        elif kind == 98:
            self.cursor = self.stack(view)
        elif kind == 107:
            self.selected = view.varint()
        elif kind == 39:
            reason = view.take(1)[0]
            number = struct.unpack('>f',view.take(4))[0]
            if reason == 3:
                self.game_mode = int(number)
        elif kind == 110:
            slot = view.varint()
            stack = self.stack(view)
            # Ordinary inventory indices 0..8 map to menu hotbar 36..44.
            if 0 <= slot <= 8:
                self.slots[36+slot] = stack
            elif 9 <= slot <= 35:
                self.slots[slot] = stack

    def join(self):
        self.send(0,wire.varint(777)+wire.string('localhost')+struct.pack('>H',self.port)+b'\x02')
        self.send(0,wire.string('ZCraftOracle')+bytes(16))
        deadline = time.monotonic()+20
        while True:
            kind,body = self.read(deadline-time.monotonic())
            if kind == 3:
                self.threshold = wire.Cursor(body).varint()
            elif kind == 2:
                break
            else:
                raise RuntimeError(f'Unexpected login packet {kind}: {body.hex()}')
        self.send(3)
        self.phase = 'configuration'
        deadline = time.monotonic()+20
        while True:
            kind,body = self.read(deadline-time.monotonic())
            if kind == 15:
                self.send(7,body)
            elif kind == 4:
                self.send(4,body)
            elif kind == 3:
                self.send(3)
                break
            elif kind == 2:
                raise RuntimeError('Configuration disconnect '+body.hex())
        self.phase = 'play'
        deadline = time.monotonic()+30
        while not (self.loaded and self.position and self.batch):
            self.handle(*self.read(deadline-time.monotonic()))
        self.send(44)
        self.send(13)
        self.sync()

    def click_packet(self, slot, button=0, mode=0, state=None):
        revision = self.state if state is None else state
        self.send(18,wire.varint(0)+wire.varint(revision)+struct.pack('>h',slot)+bytes([button])+wire.varint(mode)+b'\x00\x00')
        self.send(13)

    def sync(self, request_full=True):
        before = self.full
        if request_full:
            self.click_packet(-1,state=-1)
        token = None
        if self.barriers:
            self.barrier_counter += 1
            token = self.barrier_counter
            self.send(41,wire.varint(1)+wire.pack_position((0,319,0))+b'\x01'+wire.varint(token))
        deadline = time.monotonic()+10
        while True:
            kind,body = self.read(deadline-time.monotonic())
            self.handle(kind,body)
            if token and kind == 4 and wire.Cursor(body).varint() == token:
                if request_full and self.full <= before:
                    raise AssertionError('Block-action acknowledgement arrived without full snapshot')
                break
            if token is None and self.full > before:
                break
        return {'slots':copy.deepcopy(self.slots),'cursor':copy.deepcopy(self.cursor),'state_id':self.state,'selected':self.selected}

    def creative(self, slot, item=None):
        stack = b'\x00' if item is None else wire.varint(item['count'])+wire.varint(self.ids[item['item']])+b'\x00\x00'
        self.send(57,struct.pack('>h',slot)+stack)


class Server:
    def __init__(self, directory, java, server, libraries, port):
        self.directory = directory
        self.lines = queue.Queue()
        (directory/'eula.txt').write_text('eula=true\n')
        (directory/'server.properties').write_text('\n'.join([
            'server-ip=127.0.0.1',f'server-port={port}','online-mode=false','enforce-secure-profile=false',
            'white-list=false','enforce-whitelist=false',
            'level-name=world','level-type=minecraft:flat','generate-structures=false','gamemode=creative',
            'view-distance=2','simulation-distance=2','max-players=1','max-tick-time=60000',
            'network-compression-threshold=-1','enable-rcon=false','enable-query=false','spawn-protection=0',
            'generator-settings={"layers":[{"height":1,"block":"minecraft:bedrock"},{"height":2,"block":"minecraft:dirt"},{"height":1,"block":"minecraft:grass_block"}],"biome":"minecraft:plains"}',
        ])+'\n')
        cp = os.pathsep.join(map(str,[server,*libraries]))
        self.process = subprocess.Popen([java,'-Xms128M','-Xmx768M','-cp',cp,'net.minecraft.server.Main','--nogui'],cwd=directory,
            stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True,encoding='utf-8',errors='replace',
            creationflags=getattr(subprocess,'CREATE_NO_WINDOW',0))
        self.thread = threading.Thread(target=self.read,daemon=True)
        self.thread.start()

    def read(self):
        with (self.directory/'server.log').open('w',encoding='utf-8') as output:
            for line in self.process.stdout:
                output.write(line)
                output.flush()
                self.lines.put(line)

    def until(self, marker, timeout=90):
        deadline = time.monotonic()+timeout
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise RuntimeError('Server exited '+str(self.process.returncode))
            try:
                line = self.lines.get(timeout=.2)
            except queue.Empty:
                continue
            if marker in line:
                return
        raise TimeoutError('Server marker not seen: '+marker)

    def command(self, text):
        self.process.stdin.write(text+'\n')
        self.process.stdin.flush()

    def mode(self, mode):
        marker = 'ZCRAFT_MODE_'+str(time.monotonic_ns())
        self.command(f'gamemode {mode} ZCraftOracle\nsay {marker}')
        self.until(marker,10)

    def capture(self, commands):
        marker = 'ZCRAFT_CAPTURE_'+str(time.monotonic_ns())
        self.command(f'say {marker}_BEGIN\n'+commands+f'\nsay {marker}_END')
        self.until(marker+'_BEGIN',10)
        lines = []
        deadline = time.monotonic()+10
        while time.monotonic() < deadline:
            try:
                line = self.lines.get(timeout=min(1,deadline-time.monotonic()))
            except queue.Empty:
                continue
            if marker+'_END' in line:
                return lines
            lines.append(line.rstrip())
        raise TimeoutError('Missing console capture end')

    def stop(self):
        forced = False
        if self.process.poll() is None:
            try:
                self.command('stop')
                self.process.wait(timeout=30)
            except (OSError,subprocess.TimeoutExpired):
                forced = True
                self.process.kill()
                self.process.wait(timeout=10)
        self.thread.join(5)
        for stream in (self.process.stdin,self.process.stdout):
            stream.close()
        return {'server_exit_code':self.process.returncode,'forced_kill':forced,'log_reader_stopped':not self.thread.is_alive()}


def json_input(path):
    with path.open('rb') as stream:
        raw = stream.read(MAX_JSON+1)
    if len(raw) > MAX_JSON:
        raise ValueError('Fixture exceeds 1 MiB')
    document = json.loads(raw)
    if document.get('schema') != 1 or document.get('minecraft') != '26.3' or document.get('protocol') != 777:
        raise ValueError('Expected schema 1 / Minecraft 26.3 / protocol 777')
    cases = document.get('cases')
    if not isinstance(cases,list) or not 1 <= len(cases) <= 48:
        raise ValueError('Expected 1..48 menu cases')
    names = set()
    for case in cases:
        if set(case) != {'name','gamemode','setup','actions','expected_before','expected_after','expected_dropped'}:
            raise ValueError('Unexpected menu case fields')
        name = case['name']
        if not isinstance(name,str) or not re.fullmatch('[a-z0-9_]{1,64}',name) or name in names:
            raise ValueError('Invalid or duplicate case name')
        names.add(name)
        if case['gamemode'] not in ('creative','survival'):
            raise ValueError('Only Creative and Survival are supported')
        setup = case['setup']
        if set(setup) != {'grid','inventory','cursor','selected'} or len(setup['grid']) != 4:
            raise ValueError('Expected four setup grid slots')
        if type(setup['selected']) is not int or not 0 <= setup['selected'] <= 8:
            raise ValueError('Selected hotbar index out of range')
        if not isinstance(setup['inventory'],dict) or any(not key.isdecimal() or not 9 <= int(key) <= 45 for key in setup['inventory']):
            raise ValueError('Invalid setup inventory slot')
        if not isinstance(case['actions'],list) or not 1 <= len(case['actions']) <= 8 or len(case['expected_after']) != len(case['actions']):
            raise ValueError('Expected 1..8 actions with matching snapshots')
        for action in case['actions']:
            if action == {'kind':'close'}:
                continue
            if set(action) != {'kind','slot','button','mode','state_policy'} or action['kind'] != 'click':
                raise ValueError('Invalid menu action')
            if any(type(action[k]) is not int for k in ('slot','button','mode')) or not (0 <= action['slot'] < 46 or action['slot'] == -999 and action['mode'] == 5) or not 0 <= action['button'] <= 40 or not 0 <= action['mode'] <= 6:
                raise ValueError('Menu action out of range')
            if action['state_policy'] not in ('current','same_stale'):
                raise ValueError('Unknown state ID policy')
    return document,raw


def validate_stacks(value, items):
    if isinstance(value,dict):
        if 'item' in value:
            if set(value) != {'item','count','components'} or value['item'] not in items or value['components'] != {}:
                raise ValueError('Only known default item stacks are supported')
            if type(value['count']) is not int or not 1 <= value['count'] <= 99:
                raise ValueError('Fixture stack count out of range')
        else:
            for child in value.values():
                validate_stacks(child,items)
    elif isinstance(value,list):
        for child in value:
            validate_stacks(child,items)


def runtime_items(directory, java, javac, server, libraries):
    cp = os.pathsep.join(map(str,[directory,server,*libraries]))
    subprocess.run([javac,'--release','25','-encoding','UTF-8','-Xlint:all,-classfile','-Werror','-classpath',cp,'-d',str(directory),str(HERE/'MenuFacts.java')],check=True,timeout=60)
    subprocess.run([java,'-classpath',cp,'MenuFacts','items.json'],cwd=directory,check=True,timeout=60)
    output = directory/'items.json'
    if output.stat().st_size > MAX_JSON:
        raise ValueError('Runtime registry facts exceed limit')
    data = json.loads(output.read_text(encoding='utf-8'))
    if (data['minecraft'],data['protocol'],data['data_version'],len(data['items'])) != ('26.3',777,5023,1658):
        raise ValueError('Wrong original runtime registry identity')
    return data['items']


def snapshot(value):
    return {'slots':copy.deepcopy(value['slots']),'cursor':copy.deepcopy(value['cursor']),'selected':value['selected']}


def check_snapshot(actual, expected, name):
    full = [None]*46
    for slot,value in expected['slots'].items():
        if not slot.isdecimal() or not 0 <= int(slot) < 46:
            raise ValueError('Expected snapshot slot out of range')
        full[int(slot)] = value
    if actual != {'slots':full,'cursor':expected['cursor'],'selected':expected['selected']}:
        raise AssertionError('Menu observation differs: '+name+' actual='+json.dumps(actual,separators=(',',':')))


def drops(lines):
    counts = Counter()
    for line in lines:
        match = re.search(r'has the following entity data: \{count: ([1-9][0-9]*), id: "(minecraft:[a-z0-9_./-]+)"\}$',line)
        if not match:
            raise ValueError('Unexpected item-entity console observation: '+line)
        counts[match[2]] += int(match[1])
    return [{'item':item,'count':count,'components':{}} for item,count in sorted(counts.items())]


def canonical(report, fixture_bytes, items):
    used = set()
    def collect(value):
        if isinstance(value,dict):
            if 'item' in value:
                used.add(value['item'])
            else:
                for child in value.values(): collect(child)
        elif isinstance(value,list):
            for child in value: collect(child)
    cases = []
    for observation in report['cases']:
        case = {key:observation[key] for key in ('name','gamemode','before','actions','after','dropped')}
        cases.append(case)
        collect(case)
    value = {'schema':1,'minecraft':'26.3','protocol':777,'data_version':5023,
             'method':'original_server_tcp','bundle_sha1':facts.BUNDLE_SHA1,'server_sha256':facts.SERVER_SHA256,
             'fixture_sha256':hashlib.sha256(fixture_bytes).hexdigest(),'items':{key:items[key] for key in sorted(used)},'cases':cases}
    encoded = (json.dumps(value,sort_keys=True,separators=(',',':'))+'\n').encode('ascii')
    if len(encoded) > MAX_JSON:
        raise ValueError('Canonical menu corpus exceeds 1 MiB')
    return encoded


def main():
    parser = argparse.ArgumentParser(description='Trace real Minecraft 26.3 InventoryMenu actions over isolated loopback TCP')
    parser.add_argument('--java',default='java')
    parser.add_argument('--javac',default='javac')
    parser.add_argument('--data',type=Path,default=ROOT/'.cache/vanilla/26.3')
    parser.add_argument('--fixtures',type=Path,default=HERE/'fixtures.json')
    parser.add_argument('--output',type=Path,default=HERE/'oracle.json')
    parser.add_argument('--accept-eula',action='store_true')
    parser.add_argument('--check',action='store_true')
    args = parser.parse_args()
    if not args.accept_eula:
        parser.error('Read https://aka.ms/MinecraftEULA and pass --accept-eula for this isolated original-server test')
    fixture,fixture_bytes = json_input(args.fixtures)
    if args.output.resolve() == args.fixtures.resolve():
        parser.error('Output must not overwrite input fixtures')
    server,libraries = facts.checked_inputs(args.data.resolve())
    cache = ROOT/'.cache/crafting-menu-oracle'
    cache.mkdir(parents=True,exist_ok=True)
    directory = Path(tempfile.mkdtemp(prefix='run-',dir=cache))
    (directory/'fixtures.json').write_bytes(fixture_bytes)
    print('Evidence: '+str(directory),flush=True)
    items = runtime_items(directory,args.java,args.javac,server,libraries)
    validate_stacks(fixture,items)
    for case in fixture['cases']:
        setup = case['setup']
        for stack in [*setup['grid'],*setup['inventory'].values(),setup['cursor']]:
            if stack and stack['count'] > items[stack['item']]['max_stack']:
                raise ValueError('Setup exceeds original item stack limit')
    ids = {key:value['id'] for key,value in items.items()}
    with socket.socket() as probe:
        probe.bind(('127.0.0.1',0))
        port = probe.getsockname()[1]
    owner = Server(directory,args.java,server,libraries,port)
    report = {'status':'running','server_pid':owner.process.pid,'address':f'127.0.0.1:{port}',
              'server_sha256':facts.SERVER_SHA256,'fixture_sha256':hashlib.sha256(fixture_bytes).hexdigest(),'cases':[]}
    client = None
    failure = None
    try:
        owner.until('Done (')
        client = Client(port,ids)
        client.join()
        a,b = snapshot(client.sync()),snapshot(client.sync())
        if a != b:
            raise AssertionError('Snapshot request changed inventory')
        for case in fixture['cases']:
            client.send(19,b'\x00')
            client.sync()
            owner.mode('creative')
            for slot in range(1,46): client.creative(slot)
            client.sync()
            owner.capture('kill @e[type=minecraft:item]')
            client.trace = []
            setup = case['setup']
            client.send(54,struct.pack('>h',setup['selected']))
            client.selected = setup['selected']
            client.sync()
            if setup['cursor']:
                client.creative(9,setup['cursor'])
                client.sync()
                client.click_packet(9)
                client.sync()
            for slot,stack in setup['inventory'].items(): client.creative(int(slot),stack)
            for slot,stack in enumerate(setup['grid'],1): client.creative(slot,stack)
            client.sync()
            owner.mode(case['gamemode'])
            start = client.sync()
            if client.game_mode != {'survival':0,'creative':1}[case['gamemode']]:
                raise AssertionError('Server game mode differs from case')
            check_snapshot(snapshot(start),case['expected_before'],case['name']+'/before')
            steps,wire_steps = [],[]
            for index,action in enumerate(case['actions']):
                if action['kind'] == 'close': client.send(19,b'\x00')
                else:
                    state = start['state_id']-1 if action['state_policy'] == 'same_stale' else client.state
                    client.click_packet(action['slot'],action['button'],action['mode'],state)
                request_full = action['kind'] == 'close' or action['mode'] != 5 or action['button'] & 3 == 2
                raw = client.sync(request_full)
                after = snapshot(raw)
                check_snapshot(after,case['expected_after'][index],case['name']+'/'+str(index))
                steps.append(after)
                wire_steps.append({'state_id':raw['state_id'],'wire_end':len(client.trace)})
            drop_lines = owner.capture('execute as @e[type=minecraft:item] run data get entity @s Item')
            dropped = drops(drop_lines)
            if dropped != case['expected_dropped']:
                raise AssertionError('Dropped-item observation differs: '+case['name']+' actual='+json.dumps(dropped))
            report['cases'].append({'name':case['name'],'gamemode':case['gamemode'],'before':snapshot(start),
                'actions':case['actions'],'after':steps,'dropped':dropped,'wire_steps':wire_steps,
                'wire':copy.deepcopy(client.trace),'drops_console':drop_lines})
            print('Verified '+case['name'],flush=True)
    except Exception as error:
        failure = f'{type(error).__name__}: {error}'
    finally:
        if client is not None: client.sock.close()
        cleanup = owner.stop()
        report['cleanup'] = cleanup
        if cleanup != {'server_exit_code':0,'forced_kill':False,'log_reader_stopped':True}:
            failure = (failure+'; ' if failure else '')+'Original server did not stop cleanly: '+str(cleanup)
        report['status'] = 'failed' if failure else 'passed'
        if failure: report['error'] = failure
        encoded = json.dumps(report,indent=2).encode('utf-8')
        if len(encoded) > MAX_REPORT:
            raise ValueError('Run evidence exceeds 16 MiB')
        (directory/'report.json').write_bytes(encoded+b'\n')
    if failure:
        raise RuntimeError(failure+'; evidence: '+str(directory))
    payload = canonical(report,fixture_bytes,items)
    if args.check:
        if args.output.stat().st_size > MAX_JSON or args.output.read_bytes() != payload:
            raise ValueError('Menu corpus differs from the original server')
    else:
        args.output.parent.mkdir(parents=True,exist_ok=True)
        args.output.write_bytes(payload)
    print(f"Verified {len(report['cases'])} original menu cases; owned server stopped with exit 0",flush=True)


if __name__ == '__main__':
    main()
