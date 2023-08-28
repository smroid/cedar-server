import 'package:cedar_flutter/cedar.pbgrpc.dart';
import 'package:grpc/grpc.dart';
import 'package:grpc/grpc_web.dart';

CedarClient getClient() {
  return CedarClient(GrpcWebClientChannel.xhr(Uri.base) as ClientChannel);
}