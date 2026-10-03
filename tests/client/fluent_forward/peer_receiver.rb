# Official Fluentd in_forward, Engine and Output APIs; no transport/parser replacement.
gem 'fluentd', '= 1.19.4'
require 'json'
require 'socket'
require 'serverengine'
require 'fluent/log'
require 'fluent/engine'
require 'fluent/plugin/in_forward'
require 'fluent/plugin/output'
$stdout.sync = true
$log = Fluent::Log.new(ServerEngine::DaemonLogger.new(STDERR, log_level: ServerEngine::DaemonLogger::ERROR))
class NetgetObserveOutput < Fluent::Plugin::Output
  Fluent::Plugin.register_output('netget_observe', self)
  def multi_workers_ready?; true; end
  def process(tag, stream)
    stream.each do |time, record|
      nano = time.respond_to?(:nsec) ? time.nsec : 0
      puts 'NETGET_RECORD ' + JSON.generate({tag: tag, timestamp: {seconds: time.to_i, nanoseconds: nano}, record: record})
    end
  end
end
Fluent::Engine.init(Fluent::SystemConfig.new)
config = <<~CONF
<source>
  @type forward
  bind 127.0.0.1
  port 0
</source>
<match **>
  @type netget_observe
</match>
CONF
Fluent::Engine.configure(Fluent::Config.parse(config, 'netget-peer.conf', '.', true))
engine_thread = Thread.new { Fluent::Engine.run }
input = Fluent::Engine.root_agent.inputs.first
input.event_loop_wait_until_start
server = input._servers.find { |s| s.proto == :tcp }.server
socket_view = Socket.for_fd(server.fileno)
socket_view.autoclose = false
puts 'NETGET_ADDR 127.0.0.1:' + socket_view.local_address.ip_port.to_s
sleep 30
Fluent::Engine.stop
engine_thread.join
