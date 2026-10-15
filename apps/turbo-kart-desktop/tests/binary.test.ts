import test from 'node:test';
import assert from 'node:assert/strict';
import { body, BinaryDecoder, frame, HEADER, MAX_BODY, PROGRESS_LANE, PROGRESS_BODY_LEN, PROGRESS_EVENT_LANE } from '../src/main/kartNet/binary';
function packet(lane:number,size:number):Buffer {
 const b=Buffer.alloc(HEADER+size);b.write('TKR2');b[4]=1;b[5]=lane;b.writeUInt32BE(1,8);b.write('a'.repeat(32),24);return b;
}
test('fixed binary lanes preserve all bytes and enforce WP3 payload bounds',()=>{
 const state=packet(1,1024);state[56]=255;assert.equal(body(state)[56],255);
 assert.throws(()=>body(packet(1,1025)));assert.doesNotThrow(()=>body(packet(2,4096)));
 assert.throws(()=>body(packet(2,4097)));assert.throws(()=>body(packet(4,1)));
 assert.throws(()=>body([1,2,3]));const bad=packet(2,0);bad.write('../wrong',24);assert.throws(()=>body(bad));
});
test('split binary framing is bounded and rejects length bombs before accumulating a body',()=>{
 const d=new BinaryDecoder(), p=packet(2,4096), wire=frame(p);
 assert.deepEqual(d.push(wire.subarray(0,3)),[]);assert.deepEqual(d.push(wire.subarray(3,80)),[]);
 assert.deepEqual(d.push(wire.subarray(80)),[p]);
 for(const n of [0,55,MAX_BODY+1,0xffffffff]) {const b=Buffer.alloc(4);b.writeUInt32BE(n);assert.throws(()=>new BinaryDecoder().push(b));}
 assert.throws(()=>new BinaryDecoder().push(Buffer.alloc(65537)));
});
test('E6.5a progress lane carries exactly one body; event lane is delivery-only',()=>{
 assert.doesNotThrow(()=>body(packet(PROGRESS_LANE,PROGRESS_BODY_LEN)));
 for(const n of [0,PROGRESS_BODY_LEN-1,PROGRESS_BODY_LEN+1]) assert.throws(()=>body(packet(PROGRESS_LANE,n)));
 const m=packet(PROGRESS_LANE,PROGRESS_BODY_LEN);m[6]=1;assert.throws(()=>body(m));
 assert.throws(()=>body(packet(PROGRESS_EVENT_LANE,20)));
 const d=packet(PROGRESS_EVENT_LANE,20);d[4]=3;assert.doesNotThrow(()=>body(d));
 assert.throws(()=>body(packet(8,0)));
});
