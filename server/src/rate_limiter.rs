use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use std::time::Instant;

struct Bucket { tokens: f64, last: Instant, burst: f64, rps: f64 }
impl Bucket { fn new(burst: f64, rps: f64) -> Self { Self { tokens: burst, last: Instant::now(), burst, rps } } fn take(&mut self) -> bool { let now=Instant::now(); self.tokens=(self.tokens+now.duration_since(self.last).as_secs_f64()*self.rps).min(self.burst); self.last=now; if self.tokens>=1.0 { self.tokens-=1.0; true } else { false } } }

#[derive(Clone, Debug)]
pub struct RateLimitConfig { pub global_rps:f64, pub global_burst:f64, pub publish_rps:f64, pub publish_burst:f64, pub max_connections_per_ip:usize, pub max_connections_per_cn:usize }
#[derive(Debug)] pub enum RateLimitResult { Allowed, RateLimited, ConnectionLimit }
impl RateLimitResult { pub fn is_allowed(&self)->bool { matches!(self, Self::Allowed) } }
#[derive(Default)] struct Connections { by_ip:HashMap<String,usize>, by_cn:HashMap<String,usize> }
pub struct RateLimiter { config:RateLimitConfig, global:Arc<RwLock<HashMap<String,Bucket>>>, publish:Arc<RwLock<HashMap<String,Bucket>>>, connections:Arc<RwLock<Connections>> }
impl RateLimiter {
 pub fn new(config:RateLimitConfig)->Self { Self { config, global:Arc::new(RwLock::new(HashMap::new())), publish:Arc::new(RwLock::new(HashMap::new())), connections:Arc::new(RwLock::new(Connections::default())) } }
 pub async fn check(&self, cn:&str, _ip:&str, is_publish:bool)->RateLimitResult { let mut g=self.global.write().await; if !g.entry(cn.into()).or_insert_with(||Bucket::new(self.config.global_burst,self.config.global_rps)).take(){return RateLimitResult::RateLimited} drop(g); if is_publish { let mut p=self.publish.write().await; if !p.entry(cn.into()).or_insert_with(||Bucket::new(self.config.publish_burst,self.config.publish_rps)).take(){return RateLimitResult::RateLimited} } RateLimitResult::Allowed }
 pub async fn check_connection(&self,cn:&str,ip:&str)->RateLimitResult { let mut c=self.connections.write().await; if self.config.max_connections_per_ip>0 && c.by_ip.get(ip).copied().unwrap_or(0)>=self.config.max_connections_per_ip{return RateLimitResult::ConnectionLimit} if self.config.max_connections_per_cn>0 && c.by_cn.get(cn).copied().unwrap_or(0)>=self.config.max_connections_per_cn{return RateLimitResult::ConnectionLimit} *c.by_ip.entry(ip.into()).or_default()+=1; *c.by_cn.entry(cn.into()).or_default()+=1; RateLimitResult::Allowed }
 pub async fn release_connection(&self,cn:&str,ip:&str){let mut c=self.connections.write().await; dec(&mut c.by_ip,ip);dec(&mut c.by_cn,cn)}
 pub async fn connection_stats(&self)->(HashMap<String,usize>,HashMap<String,usize>){let c=self.connections.read().await;(c.by_ip.clone(),c.by_cn.clone())}
}
fn dec(m:&mut HashMap<String,usize>,k:&str){if let Some(v)=m.get_mut(k){if *v>1{*v-=1}else{m.remove(k);}}}
