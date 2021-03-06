using System;
using System.Threading.Tasks;
using System.IO;
using Grpc.Core;
using System.Threading;

using DotNet.Types;
using DotNet.Service;

namespace dotnet_grpc
{
    class GrpcServer
    {
        Server server;

        public GrpcServer(Arguments args)
        {
            var credentials = args.GetServerCredentials();

            server = new Server
            {
                Services = { TestService.BindService(new TestServiceImpl()) },
                Ports = { new ServerPort("localhost", args.ServerPort, credentials) },
            };
            server.Start();
            Console.WriteLine($"C# Test Server running in port {args.ServerPort}");
            Console.WriteLine($" - TLS: {credentials != ServerCredentials.Insecure}");
        }

        public async Task Stop()
        {
            Console.Write("Stopping server...");
            await server.ShutdownAsync();
            Console.WriteLine(" Done.");
        }

        class TestServiceImpl : TestService.TestServiceBase
        {
            public override Task<HelloWorldResponse> HelloWorld(HelloWorldRequest request, ServerCallContext context)
            {
                return Task.FromResult(new HelloWorldResponse
                {
                    Hello = new HelloWorld
                    {
                        Greeting = "Hello",
                        Name = request.Name,
                    }
                });
            }
        }
    }
}
