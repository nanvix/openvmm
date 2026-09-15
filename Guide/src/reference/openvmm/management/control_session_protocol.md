# Control-session protocol

The microVM control console uses an aligned record stream between the guest,
OpenVMM's broker, and the authenticated host endpoint. A broker is configured
for exactly one protocol version. It does not negotiate or downgrade based on
peer input.

## Header

Every record has a 44-byte little-endian header:

| Offset | Size | Field |
| --- | ---: | --- |
| 0 | 4 | ASCII magic `NVXS` |
| 4 | 2 | protocol version |
| 6 | 1 | record type |
| 7 | 1 | flags, currently zero |
| 8 | 16 | broker instance ID |
| 24 | 8 | epoch |
| 32 | 8 | direction-local sequence |
| 40 | 4 | payload length |

`GUEST_ATTACH` and `HOST_ATTACH` are bootstrap records and use zero instance,
epoch, and sequence fields. Post-bootstrap records use the current instance
and epoch. DATA payloads contain between 1 and 65,536 bytes.

Version 1 is the compatibility default. Its record types and payload rules are
frozen, including its zero-length ACK.

## Version 2 receive credit

Version 2 retains the header and existing record type numbers. It adds record
type 9, `CREDIT`, and changes only the ACK payload:

* ACK contains one four-byte little-endian initial receive window.
* CREDIT contains one four-byte little-endian increment.
* Credit counts DATA payload bytes, not record headers.
* The initial window is from 65,536 through 4,194,314 bytes.
* A CREDIT increment is nonzero and cannot make available credit exceed the
  initial window.

The guest sends ACK only after the replacement consumer and its bounded ingress
state are ready. ACK, CREDIT, and guest DATA share the ordered guest-to-broker
sequence. CREDIT is generation-scoped by the instance and epoch fields and is
not forwarded to the host.

The broker reserves credit for an entire DATA payload before enqueueing any
part of that record toward the guest. A record that has started physical
transmission remains prepaid and finishes to preserve stream alignment.
Unstarted DATA from a retired epoch is discarded.

When credit is exhausted, one complete host record may remain in the broker's
bounded pending slot. This is ordinary backpressure, not a protocol failure.
Transport disconnect polling, RESET generation, and control-record output do
not require DATA credit.

On an active host disconnect, the broker advances the epoch, clears usable
credit, drops pending and unstarted DATA, finishes any physically started
record, and then emits RESET. The replacement epoch receives fresh credit only
from its ACK. Device reset and snapshot restore follow the same no-credit-reuse
rule. Broker saved-state schema 2 remains the version-1 format; version 2 uses
schema 3 and persists only the selected protocol version, not usable credit.

Malformed lengths or credit values, wrong-state records, wrong instance or
epoch values, and duplicate or out-of-order sequences fail closed. Existing
peer-identity and capability authentication happen before host DATA is
accepted.

## Buffer accounting

The advertised credit window bounds bytes charged to guest ingress, including
bytes in transport, parsing, or the guest's pending ingress buffer until the
guest releases them. Implementations may account a fixed inner duplex
separately; the NVX guest uses a 128-KiB inner duplex. Credit must be returned
only when charged bytes leave the bounded ingress buffer, not merely when the
outer record is read.

OpenVMM separately bounds each output queue to 16 queued records and 1 MiB,
plus one current record. Its host input uses one bounded parser, one pending
record, and a 4-KiB transport slab.
