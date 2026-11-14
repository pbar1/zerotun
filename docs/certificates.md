# Relay certificates

Both relays require a trusted CA and the exact certificate of their peer.
The edge certificate needs `serverAuth` and a DNS SAN matching `--server-name`.
The origin certificate needs `clientAuth`. PEM certificate chains must be leaf
first. Application TLS, if used, has its own certificates; relays do not need them.

## Create a private CA and relay identities

These commands require OpenSSL 3. Replace `edge.example.com` with the name you
will pass to `--server-name`; it need not resolve in DNS because `--edge` takes
a numeric IP address.

```sh
umask 077
mkdir -p credentials
cd credentials

openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -noenc \
  -keyout ca.key -out ca.pem -days 365 -subj '/CN=zerotun CA' \
  -addext 'basicConstraints=critical,CA:TRUE' \
  -addext 'keyUsage=critical,keyCertSign,cRLSign'

openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 -noenc \
  -keyout edge.key -out edge.csr -subj '/CN=edge.example.com' \
  -addext 'subjectAltName=DNS:edge.example.com' \
  -addext 'keyUsage=critical,digitalSignature' \
  -addext 'extendedKeyUsage=serverAuth'
openssl x509 -req -in edge.csr -CA ca.pem -CAkey ca.key -CAcreateserial \
  -out edge.pem -days 90 -copy_extensions copy

openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 -noenc \
  -keyout origin.key -out origin.csr -subj '/CN=zerotun origin' \
  -addext 'keyUsage=critical,digitalSignature' \
  -addext 'extendedKeyUsage=clientAuth'
openssl x509 -req -in origin.csr -CA ca.pem -CAkey ca.key -CAcreateserial \
  -out origin.pem -days 90 -copy_extensions copy
```

## Install

Copy `ca.pem`, `edge.pem` and `origin.pem` to both machines. Copy `edge.key` only
to the edge and `origin.key` only to the origin. Keep `ca.key` on the issuing
machine and keep all private keys readable only by their owner.

The [README commands](../README.md#usage) assume these files are in the working
directory. `--peer-cert` points to the other relay's certificate, in addition
to the CA verification performed through `--ca`.

Certificates are loaded at startup. To replace an identity, update that relay's
certificate and key, update the other relay's peer pin, and restart both sides.
