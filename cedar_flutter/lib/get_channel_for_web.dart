import 'package:grpc/grpc.dart';
import 'package:grpc/grpc_web.dart';

ClientChannel getChannel() {
  return GrpcWebClientChannel.xhr(Uri.base) as ClientChannel;
}
