// Test harness for unmodified Apache-2.0 OpenDNP3 3.1.2. No NetGet wire codec.
#include <opendnp3/DNP3Manager.h>
#include <opendnp3/master/DefaultMasterApplication.h>
#include <opendnp3/master/ISOEHandler.h>
#include <opendnp3/outstation/DefaultOutstationApplication.h>
#include <opendnp3/outstation/SimpleCommandHandler.h>
#include <opendnp3/outstation/UpdateBuilder.h>
#include <condition_variable>
#include <mutex>
#include <iostream>
#include <string>
#include <chrono>
using namespace opendnp3;
struct Measurements:ISOEHandler {
 std::mutex mu;std::condition_variable cv;bool binary=false,analog=false,counter=false,event=false;
 void BeginFragment(const ResponseInfo&)override{}
 void EndFragment(const ResponseInfo&)override{cv.notify_all();}
 void Process(const HeaderInfo& info,const ICollection<Indexed<Binary>>& vals)override{std::lock_guard<std::mutex>lock(mu);vals.ForeachItem([&](const Indexed<Binary>& p){if(p.value.value)binary=true;else if(p.value.time.value==123456)event=true;});}
 void Process(const HeaderInfo&,const ICollection<Indexed<Analog>>& vals)override{std::lock_guard<std::mutex>lock(mu);vals.ForeachItem([&](const Indexed<Analog>&p){if(p.value.value==12.5)analog=true;});}
 void Process(const HeaderInfo&,const ICollection<Indexed<Counter>>& vals)override{std::lock_guard<std::mutex>lock(mu);vals.ForeachItem([&](const Indexed<Counter>&p){if(p.value.value==42)counter=true;});}
 #define IGNORE(T) void Process(const HeaderInfo&,const ICollection<Indexed<T>>&)override{}
 IGNORE(DoubleBitBinary) IGNORE(FrozenCounter) IGNORE(BinaryOutputStatus) IGNORE(AnalogOutputStatus) IGNORE(OctetString) IGNORE(TimeAndInterval) IGNORE(BinaryCommandEvent) IGNORE(AnalogCommandEvent)
 void Process(const HeaderInfo&,const ICollection<DNPTime>&)override{}
};
int main(int argc,char**argv){if(argc!=4)return 2;std::string role=argv[1],host=argv[2];uint16_t port=std::stoi(argv[3]);DNP3Manager manager(1);
 if(role=="client"){
 auto channel=manager.AddTCPClient("peer",levels::NOTHING,ChannelRetry::Default(),{IPEndpoint(host,port)},"0.0.0.0",nullptr);
 MasterStackConfig config;config.link.LocalAddr=1;config.link.RemoteAddr=10;config.master.startupIntegrityClassMask=ClassField::None();config.master.unsolClassMask=ClassField::None();config.master.disableUnsolOnStartup=true;config.master.responseTimeout=TimeDuration::Seconds(3);
 auto soe=std::make_shared<Measurements>();auto master=channel->AddMaster("peer",soe,DefaultMasterApplication::Create(),config);auto scan=master->AddClassScan(ClassField::AllClasses(),TimeDuration::Seconds(60),soe);master->Enable();
 {std::unique_lock<std::mutex>lock(soe->mu);if(!soe->cv.wait_for(lock,std::chrono::seconds(8),[&]{return soe->binary&&soe->analog&&soe->counter&&soe->event;}))return 3;}
 std::mutex mu;std::condition_variable cv;bool done=false,success=false;
 master->DirectOperate(ControlRelayOutputBlock(OperationType::LATCH_ON),0,[&](const ICommandTaskResult&r){std::lock_guard<std::mutex>lock(mu);success=r.summary==TaskCompletion::SUCCESS;r.ForeachItem([&](const CommandPointResult&p){if(p.status!=CommandStatus::SUCCESS)success=false;});done=true;cv.notify_all();});
 {std::unique_lock<std::mutex>lock(mu);if(!cv.wait_for(lock,std::chrono::seconds(8),[&]{return done;})||!success)return 4;}
 std::cout<<"{\"binary\":true,\"analog\":true,\"counter\":true,\"event_time\":123456,\"control\":true}"<<std::endl;
 }else{
 auto channel=manager.AddTCPServer("peer",levels::NOTHING,ServerAcceptMode::CloseExisting,IPEndpoint(host,port),nullptr);
 DatabaseConfig db;db.binary_input[0].svariation=StaticBinaryVariation::Group1Var2;db.binary_input[0].evariation=EventBinaryVariation::Group2Var2;db.analog_input[0].svariation=StaticAnalogVariation::Group30Var5;db.counter[0].svariation=StaticCounterVariation::Group20Var1;
 OutstationStackConfig config(db);config.link.LocalAddr=10;config.link.RemoteAddr=1;config.outstation.params.allowUnsolicited=false;config.outstation.eventBufferConfig=EventBufferConfig::AllTypes(10);
 auto out=channel->AddOutstation("peer",SuccessCommandHandler::Create(),DefaultOutstationApplication::Create(),config);out->Enable();UpdateBuilder values;values.Update(Binary(true,Flags(1),DNPTime(123456)),0);values.Update(Analog(12.5),0);values.Update(Counter(42),0);out->Apply(values.Build());
 std::cout<<"{\"port\":"<<port<<"}"<<std::endl;std::string ignored;std::getline(std::cin,ignored);
 }return 0;
}
