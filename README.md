# Penoxy
`Penoxy` is a proxy software using the QUIC protocol. It provides a secure bi-directional proxy service between the server and clients, which can be used to expose intranet ports to the internet or make network services accessible on the client/server.
## Usage
### TLS configure
Currently, for security reasons, a TLS certificate must be provided. To generate a self-signed certificate, you can execute the following commands (requires `OpenSSL`):
```shell
openssl req -x509 -newkey rsa:4096 -keyout key.pem -out cert.pem -sha256 -days 3650 -nodes -subj "/C=XX/ST=StateName/L=CityName/O=CompanyName/OU=CompanySectionName/CN=CommonNameOrHostname"
openssl x509 -outform der -in cert.pem -out cert.der
openssl rsa -outform der -in key.pem -out key.der
```
Through these commands, you will acquire a pair of certificates that will expire in 10 years.
### Server/Client configure
To run the proxy, a configuration file (`penoxy_client.yml` or `penoxy.yml`) should be placed in the working directory. An example is provided in the repository. Note that it is recommended to set a complex passkey (no shorter than 32 symbols) for each host to avoid potential risks. Then the proxy can be started by directly running the executable file. And the log files can be found in the working directory.