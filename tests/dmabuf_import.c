// DMA-BUF import test: Vulkan allocates NV12/P010/444P/Q416/BGRA (linear or block-linear), VA encodes it.
// usage: imp <render node> <codec h264|hevc> <fmt nv12|p010|444p|q416|bgra> <src put|linear|bl> <w> <h> <frames> <out> [cycles]
#define VK_NO_PROTOTYPES
#include <vulkan/vulkan.h>
#include <va/va.h>
#include <va/va_drm.h>
#include <va/va_drmcommon.h>
#include <va/va_enc_h264.h>
#include <va/va_enc_hevc.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>
#include <dlfcn.h>
#include <drm_fourcc.h>
#define CHECK(call) do { VAStatus status=(call); if(status) {fprintf(stderr,"%s: %s\n",#call,vaErrorStr(status));exit(1);} } while(0)
#define VK(call) do { VkResult r=(call); if(r){fprintf(stderr,"%s: %d\n",#call,r);exit(1);} } while(0)
static PFN_vkGetInstanceProcAddr gipa; static PFN_vkGetDeviceProcAddr gdpa;
static VkInstance inst; static VkPhysicalDevice pd; static VkDevice dev; static VkQueue queue; static uint32_t qfi;
#define IP(n) PFN_##n n=(PFN_##n)gipa(inst,#n)
#define DP(n) PFN_##n n=(PFN_##n)gdpa(dev,#n)
static int w,h,hevc,bits=10000000;
static const char *fmt;
static int bpp(void){return !strcmp(fmt,"p010")||!strcmp(fmt,"q416")?2:1;}
static int full(void){return !strcmp(fmt,"444p")||!strcmp(fmt,"q416");}
static void sample(unsigned char *plane,size_t i,int v){if(bpp()==1)plane[i]=v;else((unsigned short*)plane)[i]=v<<8|(v&0xc0);}
static void pattern(unsigned char *y,unsigned char *uv,unsigned char *bgra,int n){
    int b=bpp();
    if(bgra){for(int j=0;j<h;j++)for(int i=0;i<w;i++){unsigned char*p=bgra+((size_t)j*w+i)*4;p[0]=(i*2+n*3+(j&7)*5)&255;p[1]=(j*2+n*5+(i&3)*9)&255;p[2]=((i+j)+n*7)&255;p[3]=255;}return;}
    for(int j=0;j<h;j++)for(int i=0;i<w;i++){int v=(i*3+j*2+n*5+((i^j)&15)*4)&255; if(b==1)y[(size_t)j*w+i]=v; else ((unsigned short*)y)[(size_t)j*w+i]=v<<8|(v&0xc0);}
    if(full()){size_t ysz=(size_t)w*h*b;for(int j=0;j<h;j++)for(int i=0;i<w;i++){sample(uv,(size_t)j*w+i,(i*5+j+n*3+((i>>2)^j)*7)&255);sample(uv+ysz,(size_t)j*w+i,(i+j*4+n*9+(i^(j>>1))*3)&255);}return;}
    for(int j=0;j<h/2;j++)for(int i=0;i<w/2;i++){int u=(i*5+j+n*3)&255,v=(i+j*4+n*9)&255; if(b==1){uv[(size_t)j*w+2*i]=u;uv[(size_t)j*w+2*i+1]=v;}else{((unsigned short*)uv)[(size_t)j*w+2*i]=u<<8;((unsigned short*)uv)[(size_t)j*w+2*i+1]=v<<8;}}
}
typedef struct { VkImage img; VkDeviceMemory mem; VkBuffer buf; VkDeviceMemory bmem; void *bmap; int fd; uint64_t mod; uint64_t size; int np; int off[3],pitch[3]; VkCommandPool pool; VkCommandBuffer cb; VkFence fence; } Vk;
static VkFormat vkfmt(void){return !strcmp(fmt,"nv12")?VK_FORMAT_G8_B8R8_2PLANE_420_UNORM:!strcmp(fmt,"p010")?VK_FORMAT_G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16:
    !strcmp(fmt,"444p")?VK_FORMAT_G8_B8_R8_3PLANE_444_UNORM:!strcmp(fmt,"q416")?VK_FORMAT_G16_B16_R16_3PLANE_444_UNORM:VK_FORMAT_B8G8R8A8_UNORM;}
static uint32_t memtype(uint32_t bits,VkMemoryPropertyFlags want){VkPhysicalDeviceMemoryProperties mp;((PFN_vkGetPhysicalDeviceMemoryProperties)gipa(inst,"vkGetPhysicalDeviceMemoryProperties"))(pd,&mp);for(uint32_t i=0;i<mp.memoryTypeCount;i++)if((bits&(1u<<i))&&(mp.memoryTypes[i].propertyFlags&want)==want)return i;fprintf(stderr,"no memtype\n");exit(1);}
static void vk_init(void){
    void *lib=dlopen("libvulkan.so.1",RTLD_NOW);gipa=dlsym(lib,"vkGetInstanceProcAddr");
    PFN_vkCreateInstance ci=(PFN_vkCreateInstance)gipa(NULL,"vkCreateInstance");
    VkApplicationInfo ai={VK_STRUCTURE_TYPE_APPLICATION_INFO,0,"imp",1,"imp",1,VK_API_VERSION_1_3};
    VkInstanceCreateInfo ic={VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,0,0,&ai};VK(ci(&ic,0,&inst));
    IP(vkEnumeratePhysicalDevices);IP(vkGetPhysicalDeviceProperties);IP(vkGetPhysicalDeviceQueueFamilyProperties);IP(vkCreateDevice);
    uint32_t n=8;VkPhysicalDevice pds[8];vkEnumeratePhysicalDevices(inst,&n,pds);
    for(uint32_t i=0;i<n;i++){VkPhysicalDeviceProperties p;vkGetPhysicalDeviceProperties(pds[i],&p);if(p.vendorID==0x10de){pd=pds[i];break;}}
    if(!pd){fprintf(stderr,"no nvidia vulkan\n");exit(1);}
    uint32_t qn=8;VkQueueFamilyProperties qp[8];vkGetPhysicalDeviceQueueFamilyProperties(pd,&qn,qp);
    for(uint32_t i=0;i<qn;i++)if(qp[i].queueFlags&VK_QUEUE_GRAPHICS_BIT){qfi=i;break;}
    float pr=1;VkDeviceQueueCreateInfo qc={VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,0,0,qfi,1,&pr};
    const char *ext[]={"VK_KHR_external_memory_fd","VK_EXT_external_memory_dma_buf","VK_EXT_image_drm_format_modifier","VK_KHR_image_format_list"};
    VkDeviceCreateInfo dc={VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,0,0,1,&qc,0,0,4,ext};VK(vkCreateDevice(pd,&dc,0,&dev));
    gdpa=(PFN_vkGetDeviceProcAddr)gipa(inst,"vkGetDeviceProcAddr");
    DP(vkGetDeviceQueue);vkGetDeviceQueue(dev,qfi,0,&queue);
}
static void vk_make(Vk *v,int linear){
    IP(vkGetPhysicalDeviceFormatProperties2);DP(vkCreateImage);DP(vkGetImageMemoryRequirements2);DP(vkAllocateMemory);DP(vkBindImageMemory);DP(vkGetMemoryFdKHR);DP(vkGetImageDrmFormatModifierPropertiesEXT);DP(vkGetImageSubresourceLayout);
    DP(vkCreateBuffer);DP(vkGetBufferMemoryRequirements);DP(vkBindBufferMemory);DP(vkMapMemory);DP(vkCreateCommandPool);DP(vkAllocateCommandBuffers);DP(vkCreateFence);
    VkDrmFormatModifierPropertiesEXT mods[32];VkDrmFormatModifierPropertiesListEXT ml={VK_STRUCTURE_TYPE_DRM_FORMAT_MODIFIER_PROPERTIES_LIST_EXT,0,32,mods};
    VkFormatProperties2 fp={VK_STRUCTURE_TYPE_FORMAT_PROPERTIES_2,&ml};vkGetPhysicalDeviceFormatProperties2(pd,vkfmt(),&fp);
    uint64_t want[32];int nw=0;
    for(uint32_t i=0;i<ml.drmFormatModifierCount;i++){
        int ok=(mods[i].drmFormatModifierTilingFeatures&VK_FORMAT_FEATURE_TRANSFER_DST_BIT)!=0;
        fprintf(stderr,"vk modifier %016llx planes=%u features=%x%s\n",(unsigned long long)mods[i].drmFormatModifier,mods[i].drmFormatModifierPlaneCount,mods[i].drmFormatModifierTilingFeatures,ok?"":" (no transfer dst)");
        if(!ok)continue; int lin=mods[i].drmFormatModifier==0; if(getenv("MOD")){if(mods[i].drmFormatModifier==strtoull(getenv("MOD"),0,16))want[nw++]=mods[i].drmFormatModifier;} else if(lin==linear) want[nw++]=mods[i].drmFormatModifier;
    }
    if(!nw){fprintf(stderr,"no suitable modifier\n");exit(1);}
    VkImageDrmFormatModifierListCreateInfoEXT dl={VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_LIST_CREATE_INFO_EXT,0,nw,want};
    VkExternalMemoryImageCreateInfo ei={VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,&dl,VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT};
    VkImageCreateInfo ic={VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,&ei,0,VK_IMAGE_TYPE_2D,vkfmt(),{w,h,1},1,1,VK_SAMPLE_COUNT_1_BIT,VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT,VK_IMAGE_USAGE_TRANSFER_DST_BIT|VK_IMAGE_USAGE_TRANSFER_SRC_BIT,VK_SHARING_MODE_EXCLUSIVE};
    VK(vkCreateImage(dev,&ic,0,&v->img));
    VkImageMemoryRequirementsInfo2 ri={VK_STRUCTURE_TYPE_IMAGE_MEMORY_REQUIREMENTS_INFO_2,0,v->img};VkMemoryRequirements2 mr={VK_STRUCTURE_TYPE_MEMORY_REQUIREMENTS_2};vkGetImageMemoryRequirements2(dev,&ri,&mr);
    VkMemoryDedicatedAllocateInfo da={VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,0,v->img};
    VkExportMemoryAllocateInfo ex={VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO,&da,VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT};
    VkMemoryAllocateInfo ma={VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,&ex,mr.memoryRequirements.size,memtype(mr.memoryRequirements.memoryTypeBits,VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT)};
    VK(vkAllocateMemory(dev,&ma,0,&v->mem));VK(vkBindImageMemory(dev,v->img,v->mem,0));
    v->size=mr.memoryRequirements.size;
    VkImageDrmFormatModifierPropertiesEXT mp={VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_PROPERTIES_EXT};VK(vkGetImageDrmFormatModifierPropertiesEXT(dev,v->img,&mp));v->mod=mp.drmFormatModifier;
    for(uint32_t i=0;i<ml.drmFormatModifierCount;i++)if(mods[i].drmFormatModifier==v->mod)v->np=mods[i].drmFormatModifierPlaneCount;
    for(int p=0;p<v->np;p++){VkImageSubresource s={VK_IMAGE_ASPECT_MEMORY_PLANE_0_BIT_EXT<<p,0,0};VkSubresourceLayout l;vkGetImageSubresourceLayout(dev,v->img,&s,&l);v->off[p]=l.offset;v->pitch[p]=l.rowPitch;}
    VkMemoryGetFdInfoKHR gf={VK_STRUCTURE_TYPE_MEMORY_GET_FD_INFO_KHR,0,v->mem,VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT};VK(vkGetMemoryFdKHR(dev,&gf,&v->fd));
    size_t bs=(size_t)w*h*4*2;VkBufferCreateInfo bc={VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,0,0,bs,VK_BUFFER_USAGE_TRANSFER_SRC_BIT,VK_SHARING_MODE_EXCLUSIVE};VK(vkCreateBuffer(dev,&bc,0,&v->buf));
    VkMemoryRequirements br;vkGetBufferMemoryRequirements(dev,v->buf,&br);VkMemoryAllocateInfo bm={VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,0,br.size,memtype(br.memoryTypeBits,VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT|VK_MEMORY_PROPERTY_HOST_COHERENT_BIT)};
    VK(vkAllocateMemory(dev,&bm,0,&v->bmem));VK(vkBindBufferMemory(dev,v->buf,v->bmem,0));VK(vkMapMemory(dev,v->bmem,0,bs,0,&v->bmap));
    VkCommandPoolCreateInfo pc={VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,0,VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,qfi};VK(vkCreateCommandPool(dev,&pc,0,&v->pool));
    VkCommandBufferAllocateInfo cb={VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,0,v->pool,VK_COMMAND_BUFFER_LEVEL_PRIMARY,1};VK(vkAllocateCommandBuffers(dev,&cb,&v->cb));
    VkFenceCreateInfo fc={VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};VK(vkCreateFence(dev,&fc,0,&v->fence));
}
static void vk_upload(Vk *v,int n){
    DP(vkBeginCommandBuffer);DP(vkEndCommandBuffer);DP(vkCmdPipelineBarrier);DP(vkCmdCopyBufferToImage);DP(vkQueueSubmit);DP(vkWaitForFences);DP(vkResetFences);DP(vkResetCommandBuffer);
    int b=bpp(),bgra=!strcmp(fmt,"bgra");
    unsigned char *base=v->bmap;size_t ysz=(size_t)w*h*b;
    if(bgra)pattern(0,0,base,n);else pattern(base,base+ysz,0,n);
    vkResetCommandBuffer(v->cb,0);VkCommandBufferBeginInfo bi={VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,0,VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT};vkBeginCommandBuffer(v->cb,&bi);
    VkImageAspectFlags asp=bgra?VK_IMAGE_ASPECT_COLOR_BIT:(VK_IMAGE_ASPECT_PLANE_0_BIT|VK_IMAGE_ASPECT_PLANE_1_BIT|(full()?VK_IMAGE_ASPECT_PLANE_2_BIT:0));
    VkImageMemoryBarrier br={VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,0,0,VK_ACCESS_TRANSFER_WRITE_BIT,VK_IMAGE_LAYOUT_UNDEFINED,VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,VK_QUEUE_FAMILY_IGNORED,VK_QUEUE_FAMILY_IGNORED,v->img,{asp,0,1,0,1}};
    vkCmdPipelineBarrier(v->cb,VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,VK_PIPELINE_STAGE_TRANSFER_BIT,0,0,0,0,0,1,&br);
    if(bgra){VkBufferImageCopy c={0,0,0,{VK_IMAGE_ASPECT_COLOR_BIT,0,0,1},{0,0,0},{w,h,1}};vkCmdCopyBufferToImage(v->cb,v->buf,v->img,VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,1,&c);}
    else if(full()){VkBufferImageCopy c[3];for(int p=0;p<3;p++)c[p]=(VkBufferImageCopy){ysz*p,0,0,{VK_IMAGE_ASPECT_PLANE_0_BIT<<p,0,0,1},{0,0,0},{w,h,1}};vkCmdCopyBufferToImage(v->cb,v->buf,v->img,VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,3,c);}
    else{VkBufferImageCopy c[2]={{0,0,0,{VK_IMAGE_ASPECT_PLANE_0_BIT,0,0,1},{0,0,0},{w,h,1}},{ysz,0,0,{VK_IMAGE_ASPECT_PLANE_1_BIT,0,0,1},{0,0,0},{w/2,h/2,1}}};vkCmdCopyBufferToImage(v->cb,v->buf,v->img,VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,2,c);}
    br.srcAccessMask=VK_ACCESS_TRANSFER_WRITE_BIT;br.dstAccessMask=0;br.oldLayout=VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;br.newLayout=VK_IMAGE_LAYOUT_GENERAL;
    vkCmdPipelineBarrier(v->cb,VK_PIPELINE_STAGE_TRANSFER_BIT,VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT,0,0,0,0,0,1,&br);
    vkEndCommandBuffer(v->cb);VkSubmitInfo si={VK_STRUCTURE_TYPE_SUBMIT_INFO,0,0,0,0,1,&v->cb};
    vkResetFences(dev,1,&v->fence);VK(vkQueueSubmit(queue,1,&si,v->fence));VK(vkWaitForFences(dev,1,&v->fence,1,~0ull));
}
static VASurfaceID import_surface(VADisplay d,Vk *v,int fd){
    VADRMPRIMESurfaceDescriptor desc={0};
    int bgra=!strcmp(fmt,"bgra");
    desc.fourcc=bgra?VA_FOURCC_ARGB:!strcmp(fmt,"nv12")?VA_FOURCC_NV12:!strcmp(fmt,"444p")?VA_FOURCC_444P:!strcmp(fmt,"q416")?VA_FOURCC_Q416:VA_FOURCC_P010;
    desc.width=w;desc.height=h;desc.num_objects=1;desc.objects[0].fd=fd;desc.objects[0].size=v->size;desc.objects[0].drm_format_modifier=v->mod;
    if(full()){ // one R8/R16 layer per plane: Q416 has no DRM fourcc
        desc.num_layers=3;for(int p=0;p<3;p++){desc.layers[p].drm_format=bpp()==2?DRM_FORMAT_R16:DRM_FORMAT_R8;desc.layers[p].num_planes=1;desc.layers[p].object_index[0]=0;desc.layers[p].offset[0]=v->off[p];desc.layers[p].pitch[0]=v->pitch[p];}
    }else{
    desc.num_layers=1;desc.layers[0].drm_format=bgra?DRM_FORMAT_ARGB8888:!strcmp(fmt,"nv12")?DRM_FORMAT_NV12:DRM_FORMAT_P010;
    desc.layers[0].num_planes=v->np;for(int p=0;p<v->np;p++){desc.layers[0].object_index[p]=0;desc.layers[0].offset[p]=v->off[p];desc.layers[0].pitch[p]=v->pitch[p];}
    }
    VASurfaceAttrib at[2]={{.type=VASurfaceAttribMemoryType,.flags=VA_SURFACE_ATTRIB_SETTABLE,.value={.type=VAGenericValueTypeInteger,.value.i=VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2}},
                           {.type=VASurfaceAttribExternalBufferDescriptor,.flags=VA_SURFACE_ATTRIB_SETTABLE,.value={.type=VAGenericValueTypePointer,.value.p=&desc}}};
    unsigned rt=bgra?VA_RT_FORMAT_RGB32:!strcmp(fmt,"nv12")?VA_RT_FORMAT_YUV420:!strcmp(fmt,"444p")?VA_RT_FORMAT_YUV444:!strcmp(fmt,"q416")?VA_RT_FORMAT_YUV444_10:VA_RT_FORMAT_YUV420_10;
    VASurfaceID s;CHECK(vaCreateSurfaces(d,rt,w,h,&s,1,at,2));return s;
}
static long rss_kb(void){FILE*f=fopen("/proc/self/statm","r");long a,b;fscanf(f,"%ld %ld",&a,&b);fclose(f);return b*4;}
static int fds(void){char c[64];int n=0;snprintf(c,64,"/proc/self/fd");FILE*p=0;(void)p;int count=0;extern char **environ;(void)environ;(void)n;
    char cmd[96];snprintf(cmd,96,"ls /proc/%d/fd | wc -l",getpid());FILE*f=popen(cmd,"r");fscanf(f,"%d",&count);pclose(f);return count;}
int main(int argc,char **argv){
    if(argc<9)return 2;
    int fd=open(argv[1],O_RDWR);hevc=!strcmp(argv[2],"hevc");fmt=argv[3];const char *src=argv[4];w=atoi(argv[5]);h=atoi(argv[6]);int frames=atoi(argv[7]);int cycles=argc>9?atoi(argv[9]):0;
    int bgra=!strcmp(fmt,"bgra"),ten=bpp()==2,c444=full(),put=!strcmp(src,"put");
    unsigned fourcc=c444?(ten?VA_FOURCC_Q416:VA_FOURCC_444P):(ten?VA_FOURCC_P010:VA_FOURCC_NV12);
    VADisplay d=vaGetDisplayDRM(fd);int major,minor;CHECK(vaInitialize(d,&major,&minor));
    VAProfile profile=hevc?(c444?(ten?VAProfileHEVCMain444_10:VAProfileHEVCMain444):ten?VAProfileHEVCMain10:VAProfileHEVCMain):VAProfileH264High;
    unsigned rt=c444?(ten?VA_RT_FORMAT_YUV444_10:VA_RT_FORMAT_YUV444):ten?VA_RT_FORMAT_YUV420_10:VA_RT_FORMAT_YUV420;
    VAConfigAttrib attrs[2]={{VAConfigAttribRTFormat,rt},{VAConfigAttribRateControl,VA_RC_CQP}};
    VAConfigID config;CHECK(vaCreateConfig(d,profile,VAEntrypointEncSlice,attrs,2,&config));
    VASurfaceAttrib sa={.type=VASurfaceAttribPixelFormat,.flags=VA_SURFACE_ATTRIB_SETTABLE,.value={.type=VAGenericValueTypeInteger,.value.i=fourcc}};
    VASurfaceID base;CHECK(vaCreateSurfaces(d,rt,w,h,&base,1,&sa,1));
    VAContextID ctx;CHECK(vaCreateContext(d,config,w,h,VA_PROGRESSIVE,&base,1,&ctx));
    Vk vk={0};if(!put){vk_init();vk_make(&vk,!strcmp(src,"linear"));fprintf(stderr,"vk image: modifier %016llx planes %d pitch %d/%d off %d/%d size %llu\n",(unsigned long long)vk.mod,vk.np,vk.pitch[0],vk.pitch[1],vk.off[0],vk.off[1],(unsigned long long)vk.size);}
    if(cycles){
        long r0=0;int f0=0;
        for(int i=0;i<cycles;i++){
            if(i==50){r0=rss_kb();f0=fds();}
            int dup_fd=dup(vk.fd);VASurfaceID s=import_surface(d,&vk,dup_fd);close(dup_fd);
            VADRMPRIMESurfaceDescriptor ex;CHECK(vaExportSurfaceHandle(d,s,VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,VA_EXPORT_SURFACE_READ_ONLY|VA_EXPORT_SURFACE_COMPOSED_LAYERS,&ex));
            for(uint32_t o=0;o<ex.num_objects;o++)close(ex.objects[o].fd);
            CHECK(vaDestroySurfaces(d,&s,1));
        }
        printf("cycles=%d rss_kb %ld -> %ld fds %d -> %d\n",cycles,r0,rss_kb(),f0,fds());return 0;
    }
    VAImageFormat *formats=calloc(vaMaxNumImageFormats(d),sizeof(*formats));int nf=0;CHECK(vaQueryImageFormats(d,formats,&nf));
    VAImageFormat format={0};for(int i=0;i<nf;i++)if(formats[i].fourcc==fourcc)format=formats[i];free(formats);
    VAImage image={0};if(put)CHECK(vaCreateImage(d,&format,w,h,&image));
    VABufferID coded;CHECK(vaCreateBuffer(d,ctx,VAEncCodedBufferType,w*h*8,1,NULL,&coded));
    VABufferID seq,rc,fr;
    if(hevc){
        VAEncSequenceParameterBufferHEVC s={0};s.general_profile_idc=c444?4:ten?2:1;s.general_level_idc=153;s.intra_period=60;s.intra_idr_period=60;s.ip_period=1;s.bits_per_second=bits;s.pic_width_in_luma_samples=w;s.pic_height_in_luma_samples=h;s.seq_fields.bits.chroma_format_idc=c444?3:1;s.seq_fields.bits.bit_depth_luma_minus8=ten?2:0;s.seq_fields.bits.bit_depth_chroma_minus8=ten?2:0;s.log2_min_luma_coding_block_size_minus3=c444;s.log2_diff_max_min_luma_coding_block_size=c444?1:3;s.log2_diff_max_min_transform_block_size=3;
        CHECK(vaCreateBuffer(d,ctx,VAEncSequenceParameterBufferType,sizeof(s),1,&s,&seq));
    }else{
        VAEncSequenceParameterBufferH264 s={0};s.level_idc=51;s.intra_period=60;s.intra_idr_period=60;s.ip_period=1;s.bits_per_second=bits;s.max_num_ref_frames=1;s.picture_width_in_mbs=(w+15)/16;s.picture_height_in_mbs=(h+15)/16;s.seq_fields.bits.chroma_format_idc=c444?3:1;s.seq_fields.bits.frame_mbs_only_flag=1;s.frame_cropping_flag=1;s.frame_crop_right_offset=(((w+15)/16)*16-w)/(c444?1:2);s.frame_crop_bottom_offset=(((h+15)/16)*16-h)/(c444?1:2);
        CHECK(vaCreateBuffer(d,ctx,VAEncSequenceParameterBufferType,sizeof(s),1,&s,&seq));
    }
    _Alignas(VAEncMiscParameterRateControl) unsigned char rate[sizeof(VAEncMiscParameterBuffer)+sizeof(VAEncMiscParameterRateControl)]={0};
    VAEncMiscParameterBuffer *m=(void*)rate;m->type=VAEncMiscParameterTypeRateControl;
    VAEncMiscParameterRateControl *r=(void*)m->data;r->bits_per_second=bits;r->target_percentage=100;r->initial_qp=26;
    CHECK(vaCreateBuffer(d,ctx,VAEncMiscParameterBufferType,sizeof(rate),1,rate,&rc));
    _Alignas(VAEncMiscParameterFrameRate) unsigned char fps[sizeof(VAEncMiscParameterBuffer)+sizeof(VAEncMiscParameterFrameRate)]={0};
    m=(void*)fps;m->type=VAEncMiscParameterTypeFrameRate;((VAEncMiscParameterFrameRate*)m->data)->framerate=30;
    CHECK(vaCreateBuffer(d,ctx,VAEncMiscParameterBufferType,sizeof(fps),1,fps,&fr));
    FILE *out=fopen(argv[8],"wb");
    int b=bpp();size_t ysz=(size_t)w*h*b;unsigned char *pix=malloc((size_t)w*h*4);
    long r0=0;int f0=0;
    for(int n=0;n<frames;n++){
        if(n==frames/10){r0=rss_kb();f0=fds();}
        VASurfaceID surface=base;
        if(put){
            VAImage im=image;void *map;CHECK(vaMapBuffer(d,im.buf,&map));
            unsigned char *yb=malloc(ysz),*uvb=malloc(ysz*2);pattern(yb,uvb,0,n);
            for(int y=0;y<h;y++)memcpy((char*)map+im.offsets[0]+y*im.pitches[0],yb+(size_t)y*w*b,w*b);
            if(c444)for(int p=1;p<3;p++)for(int y=0;y<h;y++)memcpy((char*)map+im.offsets[p]+y*im.pitches[p],uvb+(p-1)*ysz+(size_t)y*w*b,w*b);
            else for(int y=0;y<h/2;y++)memcpy((char*)map+im.offsets[1]+y*im.pitches[1],uvb+(size_t)y*w*b,w*b);
            free(yb);free(uvb);
            CHECK(vaUnmapBuffer(d,im.buf));CHECK(vaPutImage(d,base,im.image_id,0,0,w,h,0,0,w,h));
        }else{
            vk_upload(&vk,n);
            int dup_fd=dup(vk.fd);surface=import_surface(d,&vk,dup_fd);close(dup_fd);
        }
        VABufferID pic,slice;
        if(hevc){
            VAEncPictureParameterBufferHEVC p={0};p.coded_buf=coded;p.pic_init_qp=26;p.pic_fields.bits.idr_pic_flag=n==0||n==45||n==105;p.pic_fields.bits.coding_type=p.pic_fields.bits.idr_pic_flag?1:2;p.pic_fields.bits.reference_pic_flag=1;
            CHECK(vaCreateBuffer(d,ctx,VAEncPictureParameterBufferType,sizeof(p),1,&p,&pic));
            VAEncSliceParameterBufferHEVC s={0};s.num_ctu_in_slice=c444?((w+31)/32)*((h+31)/32):((w+63)/64)*((h+63)/64);s.slice_type=p.pic_fields.bits.idr_pic_flag?2:1;
            CHECK(vaCreateBuffer(d,ctx,VAEncSliceParameterBufferType,sizeof(s),1,&s,&slice));
        }else{
            VAEncPictureParameterBufferH264 p={0};p.coded_buf=coded;p.pic_init_qp=26;p.frame_num=n%60;p.pic_fields.bits.idr_pic_flag=n==0||n==45||n==105;p.pic_fields.bits.reference_pic_flag=1;
            CHECK(vaCreateBuffer(d,ctx,VAEncPictureParameterBufferType,sizeof(p),1,&p,&pic));
            VAEncSliceParameterBufferH264 s={0};s.num_macroblocks=((w+15)/16)*((h+15)/16);s.slice_type=p.pic_fields.bits.idr_pic_flag?2:0;
            CHECK(vaCreateBuffer(d,ctx,VAEncSliceParameterBufferType,sizeof(s),1,&s,&slice));
        }
        VABufferID buffers[]={seq,rc,fr,pic,slice};CHECK(vaBeginPicture(d,ctx,surface));CHECK(vaRenderPicture(d,ctx,buffers,5));CHECK(vaEndPicture(d,ctx));CHECK(vaSyncSurface(d,surface));
        VACodedBufferSegment *segment;CHECK(vaMapBuffer(d,coded,(void**)&segment));for(;segment;segment=segment->next)if(fwrite(segment->buf,segment->size,1,out)!=1)return 1;CHECK(vaUnmapBuffer(d,coded));CHECK(vaDestroyBuffer(d,pic));CHECK(vaDestroyBuffer(d,slice));
        if(!put)CHECK(vaDestroySurfaces(d,&surface,1));
    }
    fclose(out);printf("frames=%d rss_kb %ld -> %ld fds %d -> %d\n",frames,r0,rss_kb(),f0,fds());
    CHECK(vaTerminate(d));return 0;
}
