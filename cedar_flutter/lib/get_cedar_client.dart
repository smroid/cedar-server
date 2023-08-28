import 'package:cedar_flutter/cedar.pbgrpc.dart';
import 'package:grpc/grpc.dart';

// For non-web deployments.
CedarClient getClient() {
  return CedarClient(ClientChannel(
    '192.168.1.133',
    port: 8080,
    options: const ChannelOptions(credentials: ChannelCredentials.insecure()),
  ));
}
