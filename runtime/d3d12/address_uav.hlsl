// Write two 20-byte records through a structured UAV made at a title address.
struct Record {
    uint4 head;
    uint tail;
};
RWStructuredBuffer<Record> records : register(u0);
[numthreads(2, 1, 1)]
void main(uint index : SV_DispatchThreadID) {
    Record record;
    record.head = uint4(0xa0000000u, 0xa0000001u, 0xa0000002u, 0xa0000003u) + index * 16u;
    record.tail = 0xa0000004u + index * 16u;
    records[index] = record;
}
