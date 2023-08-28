import 'package:grpc/grpc.dart';

// For non-web deployments.
ClientChannel getChannel() {
  return ClientChannel(
    '192.168.1.133',
    port: 8080,
    options: const ChannelOptions(credentials: ChannelCredentials.insecure()),
  );
}
